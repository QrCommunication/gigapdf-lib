//! Regression: a repacked CIDFont (Type0/CIDFontType2) subset ships a *stale*
//! embedded `cmap` that maps each Unicode to the original — now blanked — glyph
//! id, while the real outlines sit at the gids reached by `code → CID → gid`.
//! `extract_font_for_web` must rebuild the served `cmap` from that authoritative
//! mapping so the browser draws the real glyphs (not hollow/wrong ones), while
//! a non-repacked sibling whose `cmap` is already correct keeps rendering.
//!
//! The reproducing fixture is a real CERFA form that carries personal data, so
//! it is deliberately NOT committed. Point the test at a local copy via the
//! `GIGAPDF_REPACKED_CID_FIXTURE` env var (it embeds `ECYBWA+TimesNewRoman` with
//! a stale cmap and `HEBIJU+TimesNewRoman` with a correct one — both Times New
//! Roman CID subsets). Absent ⇒ the test skips (so CI never needs the PII PDF).

use gigapdf_core::font::truetype::TrueTypeFont;
use gigapdf_core::Document;
use std::path::PathBuf;

/// Local path to a PII-free repacked-CID-subset PDF, or `None` to skip.
fn fixture_path() -> Option<PathBuf> {
    let p = std::env::var_os("GIGAPDF_REPACKED_CID_FIXTURE").map(PathBuf::from)?;
    p.exists().then_some(p)
}

/// Every letter of the painted run must resolve, through the served font's
/// rebuilt `cmap`, to a glyph id with a real (non-empty) outline.
fn assert_letters_solid(doc: &Document, base_font: &str) {
    let (bytes, tag) = doc
        .extract_font_for_web(base_font)
        .unwrap_or_else(|| panic!("no web font for {base_font}"));
    assert_eq!(tag, "truetype", "{base_font} should serve a TrueType face");
    let ttf = TrueTypeFont::parse(&bytes).expect("served font parses");

    // "om et adresse de l'organisme d'assurance maladie" → its distinct letters
    // (ASCII; accented gaps not in `/ToUnicode` are out of scope here).
    for ch in "ometadrsl'inuce".chars() {
        let gid = ttf
            .gid_for_unicode(ch as u32)
            .unwrap_or_else(|| panic!("{base_font}: '{ch}' has no cmap entry"));
        assert_ne!(gid, 0, "{base_font}: '{ch}' maps to .notdef");
        assert!(
            !ttf.glyph_is_empty(gid),
            "{base_font}: '{ch}' (U+{:04X}) maps to EMPTY glyph {gid}",
            ch as u32
        );
        assert!(
            !ttf.glyph_polygons(gid).is_empty(),
            "{base_font}: '{ch}' glyph {gid} has no contours",
            ch = ch
        );
    }
}

#[test]
fn repacked_cid_subset_serves_real_glyphs() {
    let Some(path) = fixture_path() else {
        eprintln!(
            "skipping repacked_cid_subset_serves_real_glyphs: set \
             GIGAPDF_REPACKED_CID_FIXTURE to a repacked-CID-subset PDF to run it"
        );
        return;
    };
    let bytes = std::fs::read(&path).expect("read fixture");
    let doc = Document::open(&bytes).expect("open fixture");
    // The repacked subset (stale embedded cmap) is the bug — must be fixed.
    assert_letters_solid(&doc, "ECYBWA+TimesNewRoman");
    // The non-repacked sibling (correct embedded cmap) must NOT regress.
    assert_letters_solid(&doc, "HEBIJU+TimesNewRoman");
}
