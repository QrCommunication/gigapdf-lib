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

/// The 14 standard PDF "base-14" font families, by `Category` of bundled
/// substitute the rasterizer should draw them with. A PDF that references one of
/// these by name (`/Helvetica`, `/Times-Roman`, `/Courier`, `/Symbol`,
/// `/ZapfDingbats`, …) embeds **no** font program — viewers are expected to have
/// the face. Our zero-network rasterizer has none of them either, so it draws
/// nothing for such a font. This enum lets [`base14_kind`] classify a
/// `/BaseFont` name so the renderer can substitute a bundled, metric-compatible
/// face **internally** (never written into the PDF).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base14 {
    /// Helvetica / Arial family (incl. Bold/Oblique). → bundled Liberation Sans.
    Sans,
    /// Times family. → metric-compatible serif (Liberation Serif when bundled).
    Serif,
    /// Courier family. → metric-compatible monospace (Liberation Mono when bundled).
    Mono,
    /// Adobe Symbol.
    Symbol,
    /// ITC Zapf Dingbats.
    ZapfDingbats,
}

/// Classify a PDF `/BaseFont` name as one of the standard base-14 families, or
/// `None` if it isn't one of them. Tolerates subset tag prefixes (`ABCDEF+`),
/// style suffixes (`-Bold`, `,Italic`, `MT`, `PSMT`, `-Oblique`) and the
/// `/Helv` / `/ZaDb` short resource names PDFs use in `/DA` strings.
///
/// Used by the rasterizer to pick a bundled substitute for a non-embedded
/// standard font (see [`bundled_program_for_base14`]). The substitute is loaded
/// **only at render time** — it is never embedded into the document.
pub fn base14_kind(base_font: &str) -> Option<Base14> {
    // Drop a subset tag prefix ("ABCDEF+Helvetica" -> "Helvetica").
    let name = base_font.split('+').next_back().unwrap_or(base_font);
    let lower = name.trim().to_ascii_lowercase();
    // Family is whatever precedes a style separator.
    let family = lower
        .split(['-', ',', ' '])
        .next()
        .unwrap_or(lower.as_str());

    // Exact short resource names first.
    match family {
        "helv" => return Some(Base14::Sans),
        "zadb" => return Some(Base14::ZapfDingbats),
        _ => {}
    }
    if family == "symbol" {
        return Some(Base14::Symbol);
    }
    if family.starts_with("zapfdingbats") || family == "dingbats" {
        return Some(Base14::ZapfDingbats);
    }
    if family.starts_with("helvetica")
        || family.starts_with("arial")
        || family == "arialmt"
        || family == "sans"
    {
        return Some(Base14::Sans);
    }
    if family.starts_with("times") || family == "timesnewroman" {
        return Some(Base14::Serif);
    }
    if family.starts_with("courier") {
        return Some(Base14::Mono);
    }
    None
}

/// Parsed bundled fallback face, parsed once per process. Returns `None` only if
/// the embedded bytes fail to parse (they don't — covered by a unit test).
fn fallback_face() -> Option<&'static crate::font::truetype::TrueTypeFont> {
    use std::sync::OnceLock;
    static FACE: OnceLock<Option<crate::font::truetype::TrueTypeFont>> = OnceLock::new();
    FACE.get_or_init(|| crate::font::truetype::TrueTypeFont::parse(FALLBACK_TTF))
        .as_ref()
}

/// A real glyph program to draw a non-embedded base-14 font with, loaded from
/// the engine's bundled faces (cached, parsed once). Returns the program by
/// **reference** — the caller clones it into a [`crate::font::GlyphSource`] for
/// the render-fonts table; **nothing is added to the PDF**.
///
/// Only Liberation Sans Regular is bundled today (to keep the WASM small), so
/// every base-14 family maps to it: a faithful, metric-compatible substitute for
/// Helvetica/Arial, and an acceptable legible fallback for Times/Courier. Symbol
/// and ZapfDingbats codes resolve through the Latin face's `cmap` via the
/// WinAnsi → Unicode bridge in the text decoder; glyphs the face lacks (e.g. the
/// Dingbats checkmark) draw as nothing rather than tofu, exactly as before — so
/// pre-built checkbox/radio appearances are unaffected.
pub fn bundled_program_for_base14(
    _kind: Base14,
) -> Option<&'static crate::font::truetype::TrueTypeFont> {
    // When Liberation Serif / Mono are bundled, branch on `_kind` here. Until
    // then the single bundled sans is the universal substitute.
    fallback_face()
}

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

    #[test]
    fn classifies_base14_names() {
        use Base14::*;
        assert_eq!(base14_kind("Helvetica"), Some(Sans));
        assert_eq!(base14_kind("Helvetica-Bold"), Some(Sans));
        assert_eq!(base14_kind("Helvetica-BoldOblique"), Some(Sans));
        assert_eq!(base14_kind("Arial"), Some(Sans));
        assert_eq!(base14_kind("ArialMT"), Some(Sans));
        assert_eq!(base14_kind("Arial,Bold"), Some(Sans));
        assert_eq!(base14_kind("ABCDEF+Helvetica"), Some(Sans));
        assert_eq!(base14_kind("Helv"), Some(Sans));
        assert_eq!(base14_kind("Times-Roman"), Some(Serif));
        assert_eq!(base14_kind("Times New Roman"), Some(Serif));
        assert_eq!(base14_kind("TimesNewRoman,Italic"), Some(Serif));
        assert_eq!(base14_kind("Courier"), Some(Mono));
        assert_eq!(base14_kind("Courier-Bold"), Some(Mono));
        assert_eq!(base14_kind("Symbol"), Some(Symbol));
        assert_eq!(base14_kind("ZapfDingbats"), Some(ZapfDingbats));
        assert_eq!(base14_kind("ZaDb"), Some(ZapfDingbats));
        // Not base-14: an embedded/real family name.
        assert_eq!(base14_kind("Roboto"), None);
        assert_eq!(base14_kind("DejaVuSans"), None);
    }

    #[test]
    fn bundled_base14_program_is_a_real_face() {
        for kind in [
            Base14::Sans,
            Base14::Serif,
            Base14::Mono,
            Base14::Symbol,
            Base14::ZapfDingbats,
        ] {
            let face = bundled_program_for_base14(kind).expect("bundled substitute exists");
            assert!(face.units_per_em() > 0.0);
        }
    }
}
