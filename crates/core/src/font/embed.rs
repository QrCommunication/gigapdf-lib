//! Helpers to embed a TrueType program as a PDF Type0 / CIDFontType2 font.
//!
//! These are the *pure* pieces — per-glyph widths in PDF glyph space, the
//! glyph→Unicode map, and the `ToUnicode` CMap stream. The object graph
//! (FontFile2 / FontDescriptor / CIDFont / Type0) is assembled in
//! [`Document::embed_truetype_font`](crate::Document::embed_truetype_font),
//! which owns id allocation. Identity-H encoding + Identity CIDToGIDMap mean a
//! CID equals a glyph id, so the content stream shows 2-byte glyph ids directly.

use super::truetype::TrueTypeFont;

/// Per-glyph advance widths scaled to the PDF 1000-unit glyph space, indexed by
/// glyph id (the `/W` source for a CIDFont).
pub fn scaled_advances(ttf: &TrueTypeFont) -> Vec<u16> {
    let scale = 1000.0 / ttf.units_per_em();
    (0..ttf.num_glyphs())
        .map(|g| (ttf.advance_width(g) * scale).round().clamp(0.0, 65535.0) as u16)
        .collect()
}

/// Map glyph id → Unicode scalar by scanning the BMP through the font's cmap.
/// The lowest code point wins when several map to one glyph. Drives `ToUnicode`
/// so extracted/copied text round-trips.
pub fn gid_to_unicode(ttf: &TrueTypeFont) -> Vec<(u16, u32)> {
    let mut map: std::collections::BTreeMap<u16, u32> = std::collections::BTreeMap::new();
    for cp in 0x20u32..=0xFFFF {
        if let Some(gid) = ttf.gid_for_unicode(cp) {
            map.entry(gid).or_insert(cp);
        }
    }
    map.into_iter().collect()
}

/// Build a `ToUnicode` CMap stream body for `(glyph id, unicode)` pairs
/// (Adobe-Identity-UCS, BMP code points).
pub fn to_unicode_cmap(pairs: &[(u16, u32)]) -> Vec<u8> {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // `beginbfchar` blocks are capped at 100 entries each.
    for chunk in pairs.chunks(100) {
        s.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for &(gid, uni) in chunk {
            let u = uni.min(0xFFFF) as u16;
            s.push_str(&format!("<{gid:04X}> <{u:04X}>\n"));
        }
        s.push_str("endbfchar\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_unicode_has_structure_and_chunks() {
        // 150 pairs → two beginbfchar blocks (100 + 50).
        let pairs: Vec<(u16, u32)> = (1..=150u16).map(|g| (g, 0x41 + g as u32)).collect();
        let cmap = String::from_utf8(to_unicode_cmap(&pairs)).unwrap();
        assert!(cmap.contains("begincmap") && cmap.contains("endcmap"));
        assert_eq!(cmap.matches("beginbfchar").count(), 2);
        assert!(cmap.contains("<0001> <0042>"), "gid1 → 'B'");
    }
}
