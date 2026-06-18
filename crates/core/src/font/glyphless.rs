//! A synthesised "glyphless" TrueType font — `N+1` empty glyphs (glyph 0 =
//! `.notdef`, then one per distinct character) with a uniform advance.
//!
//! Used by [`crate::document::Document::add_text_layer`] to carry an arbitrary
//! Unicode OCR text layer in render mode `3 Tr` (invisible). The glyphs are
//! never painted, so giving them no outlines keeps the embedded program tiny
//! while a `/ToUnicode` CMap (built by the caller) makes the text searchable
//! and copyable regardless of script (Cyrillic, Greek, Arabic, Bengali…).
//!
//! The sfnt is laid out so [`crate::font::truetype::TrueTypeFont::parse`]
//! accepts it: it carries the four tables that reader requires (`head`,
//! `maxp`, `glyf`, `loca`) plus `hhea`, `hmtx` and a minimal `cmap`.

use super::cff_to_otf::assemble_sfnt;

/// Uniform glyph advance, in font units (`unitsPerEm = 1000`), i.e. 0.5 em.
const GLYPH_ADVANCE: u16 = 500;

/// Build a glyphless TrueType program with `num_glyphs` empty glyphs.
///
/// `num_glyphs` is the total glyph count (glyph 0 = `.notdef` plus one per
/// distinct mapped character). It must be ≥ 1; callers pass `N + 1`.
pub fn build_glyphless_ttf(num_glyphs: u16) -> Vec<u8> {
    let n = num_glyphs.max(1);
    // sfnt 1.0 ("true" outlines). Tables must be alphabetically sorted for the
    // directory; `assemble_sfnt` patches head.checkSumAdjustment afterwards.
    let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"cmap", build_cmap()),
        (b"glyf", Vec::new()), // every glyph is empty → no glyf data at all
        (b"head", build_head()),
        (b"hhea", build_hhea(n)),
        (b"hmtx", build_hmtx(n)),
        (b"loca", build_loca(n)),
        (b"maxp", build_maxp(n)),
    ];
    tables.sort_by(|a, b| a.0.cmp(b.0));
    assemble_sfnt(0x0001_0000, &mut tables)
}

/// `head` with `indexToLocFormat = 0` (short `loca`) and `unitsPerEm = 1000`.
fn build_head() -> Vec<u8> {
    let mut t = Vec::with_capacity(54);
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version 1.0
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // fontRevision 1.0
    t.extend_from_slice(&0u32.to_be_bytes()); // checkSumAdjustment (patched later)
    t.extend_from_slice(&0x5F0F_3CF5u32.to_be_bytes()); // magicNumber
    t.extend_from_slice(&0x000Bu16.to_be_bytes()); // flags
    t.extend_from_slice(&1000u16.to_be_bytes()); // unitsPerEm
    t.extend_from_slice(&0u64.to_be_bytes()); // created
    t.extend_from_slice(&0u64.to_be_bytes()); // modified
    t.extend_from_slice(&0i16.to_be_bytes()); // xMin
    t.extend_from_slice(&(-200i16).to_be_bytes()); // yMin
    t.extend_from_slice(&1000i16.to_be_bytes()); // xMax
    t.extend_from_slice(&800i16.to_be_bytes()); // yMax
    t.extend_from_slice(&0u16.to_be_bytes()); // macStyle
    t.extend_from_slice(&8u16.to_be_bytes()); // lowestRecPPEM
    t.extend_from_slice(&2i16.to_be_bytes()); // fontDirectionHint
    t.extend_from_slice(&0i16.to_be_bytes()); // indexToLocFormat (short)
    t.extend_from_slice(&0i16.to_be_bytes()); // glyphDataFormat
    t
}

/// `hhea` v1.0. `numberOfHMetrics = num_glyphs` (one hmtx entry per glyph).
fn build_hhea(num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(36);
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version 1.0
    t.extend_from_slice(&800i16.to_be_bytes()); // ascender
    t.extend_from_slice(&(-200i16).to_be_bytes()); // descender
    t.extend_from_slice(&0i16.to_be_bytes()); // lineGap
    t.extend_from_slice(&GLYPH_ADVANCE.to_be_bytes()); // advanceWidthMax
    t.extend_from_slice(&0i16.to_be_bytes()); // minLeftSideBearing
    t.extend_from_slice(&0i16.to_be_bytes()); // minRightSideBearing
    t.extend_from_slice(&(GLYPH_ADVANCE as i16).to_be_bytes()); // xMaxExtent
    t.extend_from_slice(&1i16.to_be_bytes()); // caretSlopeRise
    t.extend_from_slice(&0i16.to_be_bytes()); // caretSlopeRun
    t.extend_from_slice(&0i16.to_be_bytes()); // caretOffset
    for _ in 0..4 {
        t.extend_from_slice(&0i16.to_be_bytes()); // reserved
    }
    t.extend_from_slice(&0i16.to_be_bytes()); // metricDataFormat
    t.extend_from_slice(&num_glyphs.to_be_bytes()); // numberOfHMetrics
    t
}

/// `hmtx`: `num_glyphs` entries, each `{advanceWidth = 500, lsb = 0}`.
fn build_hmtx(num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(num_glyphs as usize * 4);
    for _ in 0..num_glyphs {
        t.extend_from_slice(&GLYPH_ADVANCE.to_be_bytes()); // advanceWidth
        t.extend_from_slice(&0i16.to_be_bytes()); // lsb
    }
    t
}

/// `maxp` **version 1.0** (TrueType-with-glyf requirement). `numGlyphs` set;
/// every other bound is 0 — correct for glyphs that contain no contours.
fn build_maxp(num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(32);
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version 1.0
    t.extend_from_slice(&num_glyphs.to_be_bytes()); // numGlyphs
    for _ in 0..13 {
        t.extend_from_slice(&0u16.to_be_bytes()); // maxPoints..maxComponentDepth = 0
    }
    t
}

/// `loca` (short format): `num_glyphs + 1` offsets, all 0 → every glyph spans
/// `[0, 0)` in `glyf`, i.e. is empty.
fn build_loca(num_glyphs: u16) -> Vec<u8> {
    vec![0u8; (num_glyphs as usize + 1) * 2]
}

/// Minimal `cmap` (format 4) — just the mandatory `0xFFFF` terminator segment.
/// Text is shown via Identity-H (CID = GID), so the embedded cmap is unused for
/// rendering; it is present only to keep the sfnt well-formed.
fn build_cmap() -> Vec<u8> {
    // One segment: end = start = 0xFFFF, idDelta = 1, idRangeOffset = 0.
    let seg_count: u16 = 1;
    let seg_x2 = seg_count * 2;
    let max_pow2 = 15u16 - seg_count.leading_zeros() as u16; // floor(log2(1)) = 0
    let search_range = (1u16 << max_pow2) * 2;
    let entry_selector = max_pow2;
    let range_shift = seg_x2 - search_range;

    let mut sub = Vec::new();
    sub.extend_from_slice(&4u16.to_be_bytes()); // format
    let length_pos = sub.len();
    sub.extend_from_slice(&0u16.to_be_bytes()); // length (patched)
    sub.extend_from_slice(&0u16.to_be_bytes()); // language
    sub.extend_from_slice(&seg_x2.to_be_bytes());
    sub.extend_from_slice(&search_range.to_be_bytes());
    sub.extend_from_slice(&entry_selector.to_be_bytes());
    sub.extend_from_slice(&range_shift.to_be_bytes());
    sub.extend_from_slice(&0xFFFFu16.to_be_bytes()); // endCount[0]
    sub.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
    sub.extend_from_slice(&0xFFFFu16.to_be_bytes()); // startCount[0]
    sub.extend_from_slice(&1i16.to_be_bytes()); // idDelta[0]
    sub.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[0]
    let len = sub.len() as u16;
    sub[length_pos..length_pos + 2].copy_from_slice(&len.to_be_bytes());

    // cmap header + one encoding record (platform 3, encoding 1) → subtable.
    let mut t = Vec::new();
    t.extend_from_slice(&0u16.to_be_bytes()); // version
    t.extend_from_slice(&1u16.to_be_bytes()); // numTables
    t.extend_from_slice(&3u16.to_be_bytes()); // platformID
    t.extend_from_slice(&1u16.to_be_bytes()); // encodingID
    t.extend_from_slice(&12u32.to_be_bytes()); // offset (4 + 8)
    t.extend_from_slice(&sub);
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::truetype::TrueTypeFont;

    #[test]
    fn glyphless_ttf_parses() {
        let ttf = build_glyphless_ttf(5);
        let font = TrueTypeFont::parse(&ttf).expect("glyphless sfnt must parse");
        assert_eq!(font.num_glyphs(), 5);
    }

    #[test]
    fn glyphless_ttf_clamps_zero_to_one_glyph() {
        // num_glyphs = 0 would underflow the sfnt directory math; we floor at 1.
        let ttf = build_glyphless_ttf(0);
        let font = TrueTypeFont::parse(&ttf).expect("min glyphless sfnt must parse");
        assert_eq!(font.num_glyphs(), 1);
    }
}
