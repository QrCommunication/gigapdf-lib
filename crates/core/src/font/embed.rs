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

/// Map glyph id → Unicode string by scanning the font's **full** cmap (all
/// planes, including supplementary code points). The lowest code point wins when
/// several map to one glyph. Drives `ToUnicode` so extracted/copied text
/// round-trips — including emoji and other astral-plane glyphs.
///
/// Each entry's string is a single character (the mapped scalar). Ligature
/// glyphs (which no code point maps to) are folded in separately by
/// [`gid_to_unicode_with_ligatures`].
pub fn gid_to_unicode(ttf: &TrueTypeFont) -> Vec<(u16, String)> {
    ttf.gid_to_unicode_map()
        .into_iter()
        .filter_map(|(gid, cp)| char::from_u32(cp).map(|c| (gid, c.to_string())))
        .collect()
}

/// Like [`gid_to_unicode`] but also maps each **ligature glyph** to the Unicode
/// of its component glyphs (resolved through the cmap), so a ligated run — `ffi`,
/// `fl`, … drawn as one glyph — still extracts and copies as the original
/// characters instead of tofu. Ligature entries override the bare cmap entry for
/// the same glyph id (a ligature glyph is never also a plain character).
pub fn gid_to_unicode_with_ligatures(
    ttf: &TrueTypeFont,
    ligatures: &[(u16, Vec<u16>)],
) -> Vec<(u16, String)> {
    let cmap = ttf.gid_to_unicode_map();
    let mut map: std::collections::BTreeMap<u16, String> = cmap
        .iter()
        .filter_map(|(&gid, &cp)| char::from_u32(cp).map(|c| (gid, c.to_string())))
        .collect();
    for (lig_gid, components) in ligatures {
        // Build the component string from each component glyph's Unicode. Skip
        // the ligature when any component has no cmap entry (can't round-trip).
        let mut s = String::new();
        let mut ok = true;
        for comp in components {
            match cmap.get(comp).and_then(|&cp| char::from_u32(cp)) {
                Some(c) => s.push(c),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && !s.is_empty() {
            map.insert(*lig_gid, s);
        }
    }
    map.into_iter().collect()
}

/// Build a `ToUnicode` CMap stream body for `(glyph id, unicode string)` pairs
/// (Adobe-Identity-UCS). Destination strings are UTF-16BE, so supplementary
/// planes (surrogate pairs) and multi-character ligature expansions round-trip.
pub fn to_unicode_cmap(pairs: &[(u16, String)]) -> Vec<u8> {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // `beginbfchar` blocks are capped at 100 entries each.
    for chunk in pairs.chunks(100) {
        s.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (gid, text) in chunk {
            s.push_str(&format!("<{gid:04X}> <{}>\n", utf16be_hex(text)));
        }
        s.push_str("endbfchar\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s.into_bytes()
}

/// Encode a string as upper-case UTF-16BE hex (surrogate pairs included), the
/// form a `/ToUnicode` `beginbfchar` destination uses.
fn utf16be_hex(text: &str) -> String {
    let mut out = String::new();
    let mut buf = [0u16; 2];
    for c in text.chars() {
        for unit in c.encode_utf16(&mut buf) {
            out.push_str(&format!("{unit:04X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_unicode_has_structure_and_chunks() {
        // 150 pairs → two beginbfchar blocks (100 + 50).
        let pairs: Vec<(u16, String)> = (1..=150u16)
            .map(|g| (g, char::from_u32(0x41 + g as u32).unwrap().to_string()))
            .collect();
        let cmap = String::from_utf8(to_unicode_cmap(&pairs)).unwrap();
        assert!(cmap.contains("begincmap") && cmap.contains("endcmap"));
        assert_eq!(cmap.matches("beginbfchar").count(), 2);
        assert!(cmap.contains("<0001> <0042>"), "gid1 → 'B'");
    }

    #[test]
    fn to_unicode_encodes_supplementary_plane_as_surrogate_pair() {
        // A glyph mapped to U+1F600 (😀) must serialise as the UTF-16BE surrogate
        // pair D83D DE00 — proof astral-plane glyphs round-trip on extraction.
        let pairs = vec![(5u16, "\u{1F600}".to_string())];
        let cmap = String::from_utf8(to_unicode_cmap(&pairs)).unwrap();
        assert!(
            cmap.contains("<0005> <D83DDE00>"),
            "emoji glyph → surrogate pair: {cmap}"
        );
    }

    #[test]
    fn to_unicode_encodes_multichar_ligature_expansion() {
        // A ligature glyph mapping back to "ffi" serialises all three UTF-16
        // units, so a ligated run still copies as the source characters.
        let pairs = vec![(7u16, "ffi".to_string())];
        let cmap = String::from_utf8(to_unicode_cmap(&pairs)).unwrap();
        assert!(
            cmap.contains("<0007> <006600660069>"),
            "ffi ligature expands to f f i: {cmap}"
        );
    }
}
