//! Wrap a **bare CFF** font program (the contents of a PDF `FontFile3`
//! `/Subtype /Type1C`) into a minimal **OpenType-CFF** (`OTTO`) sfnt, so the
//! engine's existing OpenType-CFF embedding path ([`Document::embed_font`]) can
//! re-embed it as a subsettable Type0 font. A bare CFF has no sfnt tables, so a
//! viewer cannot map characters to glyphs; this module synthesises the missing
//! `cmap`/`head`/`hhea`/`hmtx`/`maxp`/`name`/`OS/2`/`post` tables from the CFF's
//! own metrics + charset, with zero external dependency.
//!
//! Character→glyph mapping is recovered from the CFF charset: each glyph's SID
//! resolves to a Unicode value (Standard Strings SID 1..=95 are ASCII
//! `0x20..=0x7E` in order; SID 96..=228 are Latin-1/special; font-specific names
//! fall back to AGL conventions). The result is a format-4 `cmap`.

use super::cff::CffFont;

/// Wrap a bare CFF program into an OpenType-CFF (`OTTO`) sfnt. Returns `None`
/// if the CFF cannot be parsed.
pub fn wrap(cff_bytes: &[u8]) -> Option<Vec<u8>> {
    let cff = CffFont::parse(cff_bytes)?;
    let num_glyphs = cff.num_glyphs();
    if num_glyphs == 0 {
        return None;
    }
    let upm = cff.units_per_em().round().clamp(16.0, 16384.0) as u16;

    // Per-glyph advance widths (font units) + the Unicode→GID pairs for the cmap.
    let mut advances: Vec<u16> = Vec::with_capacity(num_glyphs as usize);
    let mut max_adv: u16 = 0;
    let mut cmap_pairs: Vec<(u32, u16)> = Vec::new();
    for gid in 0..num_glyphs {
        let adv = cff.advance_width(gid).round().clamp(0.0, 65535.0) as u16;
        advances.push(adv);
        max_adv = max_adv.max(adv);
        if gid != 0 {
            if let Some(cp) = sid_to_unicode(&cff, cff.gid_to_sid(gid)) {
                if cp <= 0xFFFF && cp != 0 {
                    cmap_pairs.push((cp, gid));
                }
            }
        }
    }
    // Keep the first GID seen for each code point (lower GIDs win).
    cmap_pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    cmap_pairs.dedup_by_key(|p| p.0);

    let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"CFF ", cff_bytes.to_vec()),
        (b"OS/2", build_os2(max_adv, num_glyphs)),
        (b"cmap", build_cmap(&cmap_pairs)),
        (b"head", build_head(upm)),
        (b"hhea", build_hhea(max_adv, num_glyphs)),
        (b"hmtx", build_hmtx(&advances)),
        (b"maxp", build_maxp(num_glyphs)),
        (b"name", build_name()),
        (b"post", build_post()),
    ];
    tables.sort_by(|a, b| a.0.cmp(b.0));

    Some(assemble_sfnt(0x4F54_544F, &mut tables))
}

/// Build a glyph-id → Unicode **string** map from a CFF font's charset glyph
/// names, resolving **ligature** glyphs (`ffi`, `fi`, …) to their multi-character
/// expansions. Drives the `/ToUnicode` CMap for a CFF-embedded font so a ligated
/// run extracts/copies as the original characters instead of tofu. CID-keyed CFF
/// has no name charset, so it yields nothing.
pub fn cff_gid_unicode_strings(cff: &CffFont) -> std::collections::BTreeMap<u16, String> {
    let mut map = std::collections::BTreeMap::new();
    if cff.is_cid() {
        return map;
    }
    for gid in 1..cff.num_glyphs() {
        let sid = cff.gid_to_sid(gid);
        // Standard Strings (SID < 391) resolve through their predefined name;
        // font-specific names come from the String INDEX. Either way, route the
        // name through the string resolver so ligatures expand.
        if let Some(name) = cff.sid_name(sid) {
            if let Some(s) = glyph_name_to_unicode_string(name) {
                map.insert(gid, s);
            }
        }
    }
    map
}

/// Resolve a glyph's charset SID to a Unicode scalar for the synthesised cmap.
fn sid_to_unicode(cff: &CffFont, sid: u16) -> Option<u32> {
    if cff.is_cid() {
        return None; // CID-keyed: the charset holds CIDs, not name SIDs.
    }
    // Standard Strings SID 1..=95 == StandardEncoding printable run 0x20..=0x7E.
    if (1..=95).contains(&sid) {
        return Some(0x1F + sid as u32);
    }
    if (96..=228).contains(&sid) {
        return latin1_sid_unicode(sid);
    }
    // Font-specific name (SID >= 391) → AGL conventions.
    glyph_name_to_unicode(cff.sid_name(sid)?)
}

/// AGL-style glyph-name → Unicode for font String INDEX names: `uniXXXX`,
/// `uXXXXXX`, and single-character names. Stylistic/unknown names → `None`.
fn glyph_name_to_unicode(raw: &str) -> Option<u32> {
    // Strip a subset prefix ("ABCDEF+name") and a ".suffix".
    let name = raw.rsplit('+').next().unwrap_or(raw);
    let name = name.split('.').next().unwrap_or(name);
    if name.is_empty() || name == "notdef" {
        return None;
    }
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() == 4 {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return Some(v);
            }
        }
    }
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return Some(v);
            }
        }
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return Some(first as u32); // single-character name → that scalar.
    }
    None
}

/// Resolve a glyph name to a Unicode **string** (not just a single scalar), so
/// **ligature** glyphs round-trip on `/ToUnicode` extraction. Handles the
/// standard Latin f-ligatures by name (`ffi`, `ffl`, `ff`, `fi`, `fl`), AGL
/// `uniXXXX(XXXX…)` runs of 4-hex units, and falls back to the single-scalar
/// resolver for everything it already knew. Stylistic/unknown names → `None`.
pub fn glyph_name_to_unicode_string(raw: &str) -> Option<String> {
    let name = raw.rsplit('+').next().unwrap_or(raw);
    let name = name.split('.').next().unwrap_or(name);
    if name.is_empty() || name == "notdef" {
        return None;
    }
    // Standard Latin ligatures (the common cases a `cmap` can't express).
    let ligature = match name {
        "ffi" => Some("ffi"),
        "ffl" => Some("ffl"),
        "ff" => Some("ff"),
        "fi" => Some("fi"),
        "fl" => Some("fl"),
        "ft" => Some("ft"),
        "st" => Some("st"),
        _ => None,
    };
    if let Some(s) = ligature {
        return Some(s.to_string());
    }
    // AGL `uniXXXXYYYY…`: a run of 4-hex-digit BMP units (ligatures are encoded
    // this way too, e.g. `uni0066006900` is not valid — must be multiples of 4).
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() >= 4 && hex.len().is_multiple_of(4) && hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            let mut s = String::new();
            let mut ok = true;
            for chunk in hex.as_bytes().chunks(4) {
                let h = std::str::from_utf8(chunk).ok()?;
                match u32::from_str_radix(h, 16).ok().and_then(char::from_u32) {
                    Some(c) => s.push(c),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && !s.is_empty() {
                return Some(s);
            }
        }
    }
    // Otherwise fall back to the single-scalar conventions (uXXXXXX, single char).
    glyph_name_to_unicode(name).and_then(char::from_u32).map(|c| c.to_string())
}

// ── sfnt assembly ───────────────────────────────────────────────────────────

/// Assemble a table directory + padded table data into a complete sfnt, fixing
/// up `head.checkSumAdjustment` once the whole font is laid out.
pub(crate) fn assemble_sfnt(sfnt_version: u32, tables: &mut [(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = tables.len() as u16;
    let max_pow2 = 15u16 - n.leading_zeros() as u16; // floor(log2(n))
    let search_range = (1u16 << max_pow2) * 16;
    let entry_selector = max_pow2;
    let range_shift = n * 16 - search_range;

    let mut out = Vec::new();
    out.extend_from_slice(&sfnt_version.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    let mut offset = 12 + tables.len() * 16;
    let mut head_record: Option<usize> = None; // dir-record offset of head
    let mut dir = Vec::new();
    let mut body = Vec::new();
    for (tag, bytes) in tables.iter() {
        let mut padded = bytes.clone();
        while !padded.len().is_multiple_of(4) {
            padded.push(0);
        }
        let checksum = table_checksum(&padded);
        if *tag == b"head" {
            head_record = Some(12 + dir.len());
        }
        dir.extend_from_slice(*tag);
        dir.extend_from_slice(&checksum.to_be_bytes());
        dir.extend_from_slice(&(offset as u32).to_be_bytes());
        dir.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(&padded);
        offset += padded.len();
    }
    out.extend_from_slice(&dir);
    out.extend_from_slice(&body);

    // head.checkSumAdjustment = 0xB1B0AFBA − checksum(entire font). The field is
    // currently zero; locate the head table's data via its directory record
    // (offset field at +8) and patch in the adjustment.
    if let Some(rec) = head_record {
        let head_off =
            u32::from_be_bytes([out[rec + 8], out[rec + 9], out[rec + 10], out[rec + 11]]) as usize;
        let adj = 0xB1B0_AFBAu32.wrapping_sub(table_checksum(&out));
        let pos = head_off + 8;
        if pos + 4 <= out.len() {
            out[pos..pos + 4].copy_from_slice(&adj.to_be_bytes());
        }
    }
    out
}

/// sfnt table checksum: sum of big-endian u32 words (zero-padded).
pub(crate) fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0;
    while i < data.len() {
        let mut word = [0u8; 4];
        for (j, w) in word.iter_mut().enumerate() {
            if i + j < data.len() {
                *w = data[i + j];
            }
        }
        sum = sum.wrapping_add(u32::from_be_bytes(word));
        i += 4;
    }
    sum
}

fn build_head(upm: u16) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version 1.0
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // fontRevision 1.0
    t.extend_from_slice(&0u32.to_be_bytes()); // checkSumAdjustment (fixed later)
    t.extend_from_slice(&0x5F0F_3CF5u32.to_be_bytes()); // magicNumber
    t.extend_from_slice(&0x000Bu16.to_be_bytes()); // flags
    t.extend_from_slice(&upm.to_be_bytes()); // unitsPerEm
    t.extend_from_slice(&0u64.to_be_bytes()); // created
    t.extend_from_slice(&0u64.to_be_bytes()); // modified
    t.extend_from_slice(&0i16.to_be_bytes()); // xMin
    t.extend_from_slice(&(-250i16).to_be_bytes()); // yMin
    t.extend_from_slice(&1000i16.to_be_bytes()); // xMax
    t.extend_from_slice(&900i16.to_be_bytes()); // yMax
    t.extend_from_slice(&0u16.to_be_bytes()); // macStyle
    t.extend_from_slice(&8u16.to_be_bytes()); // lowestRecPPEM
    t.extend_from_slice(&2i16.to_be_bytes()); // fontDirectionHint
    t.extend_from_slice(&0i16.to_be_bytes()); // indexToLocFormat
    t.extend_from_slice(&0i16.to_be_bytes()); // glyphDataFormat
    t
}

fn build_hhea(max_adv: u16, num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version 1.0
    t.extend_from_slice(&800i16.to_be_bytes()); // ascender
    t.extend_from_slice(&(-200i16).to_be_bytes()); // descender
    t.extend_from_slice(&0i16.to_be_bytes()); // lineGap
    t.extend_from_slice(&max_adv.to_be_bytes()); // advanceWidthMax
    t.extend_from_slice(&0i16.to_be_bytes()); // minLeftSideBearing
    t.extend_from_slice(&0i16.to_be_bytes()); // minRightSideBearing
    t.extend_from_slice(&(max_adv as i16).to_be_bytes()); // xMaxExtent
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

fn build_hmtx(advances: &[u16]) -> Vec<u8> {
    let mut t = Vec::with_capacity(advances.len() * 4);
    for &adv in advances {
        t.extend_from_slice(&adv.to_be_bytes()); // advanceWidth
        t.extend_from_slice(&0i16.to_be_bytes()); // lsb
    }
    t
}

fn build_maxp(num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&0x0000_5000u32.to_be_bytes()); // version 0.5 (CFF)
    t.extend_from_slice(&num_glyphs.to_be_bytes());
    t
}

fn build_post() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&0x0003_0000u32.to_be_bytes()); // version 3.0 (no names)
    t.extend_from_slice(&0i32.to_be_bytes()); // italicAngle
    t.extend_from_slice(&(-100i16).to_be_bytes()); // underlinePosition
    t.extend_from_slice(&50i16.to_be_bytes()); // underlineThickness
    t.extend_from_slice(&0u32.to_be_bytes()); // isFixedPitch
    t.extend_from_slice(&0u32.to_be_bytes()); // minMemType42
    t.extend_from_slice(&0u32.to_be_bytes()); // maxMemType42
    t.extend_from_slice(&0u32.to_be_bytes()); // minMemType1
    t.extend_from_slice(&0u32.to_be_bytes()); // maxMemType1
    t
}

fn build_os2(max_adv: u16, _num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(96);
    t.extend_from_slice(&4u16.to_be_bytes()); // version 4
    t.extend_from_slice(&((max_adv / 2) as i16).to_be_bytes()); // xAvgCharWidth
    t.extend_from_slice(&400u16.to_be_bytes()); // usWeightClass
    t.extend_from_slice(&5u16.to_be_bytes()); // usWidthClass
    t.extend_from_slice(&0u16.to_be_bytes()); // fsType
    t.extend_from_slice(&[0u8; 20]); // sub/superscript + strikeout metrics (10×i16)
    t.extend_from_slice(&0i16.to_be_bytes()); // sFamilyClass
    t.extend_from_slice(&[0u8; 10]); // panose
    t.extend_from_slice(&[0u8; 16]); // ulUnicodeRange1..4
    t.extend_from_slice(b"GPDF"); // achVendID
    t.extend_from_slice(&0x0040u16.to_be_bytes()); // fsSelection (REGULAR)
    t.extend_from_slice(&0x20u16.to_be_bytes()); // usFirstCharIndex
    t.extend_from_slice(&0xFFFFu16.to_be_bytes()); // usLastCharIndex
    t.extend_from_slice(&800i16.to_be_bytes()); // sTypoAscender
    t.extend_from_slice(&(-200i16).to_be_bytes()); // sTypoDescender
    t.extend_from_slice(&0i16.to_be_bytes()); // sTypoLineGap
    t.extend_from_slice(&900u16.to_be_bytes()); // usWinAscent
    t.extend_from_slice(&250u16.to_be_bytes()); // usWinDescent
    t.extend_from_slice(&[0u8; 8]); // ulCodePageRange1..2
    t.extend_from_slice(&500i16.to_be_bytes()); // sxHeight
    t.extend_from_slice(&700i16.to_be_bytes()); // sCapHeight
    t.extend_from_slice(&0u16.to_be_bytes()); // usDefaultChar
    t.extend_from_slice(&0x20u16.to_be_bytes()); // usBreakChar
    t.extend_from_slice(&0u16.to_be_bytes()); // usMaxContext
    t
}

fn build_name() -> Vec<u8> {
    // One record (nameID 6, PostScript name) in platform 3 / encoding 1 / 0x409.
    let value: Vec<u16> = "GigaPDFCFF".encode_utf16().collect();
    let mut string_data = Vec::new();
    for u in &value {
        string_data.extend_from_slice(&u.to_be_bytes());
    }
    let mut t = Vec::new();
    t.extend_from_slice(&0u16.to_be_bytes()); // format 0
    t.extend_from_slice(&1u16.to_be_bytes()); // count
    t.extend_from_slice(&(6u16 + 12).to_be_bytes()); // stringOffset (header + 1 record)
    // record
    t.extend_from_slice(&3u16.to_be_bytes()); // platformID
    t.extend_from_slice(&1u16.to_be_bytes()); // encodingID
    t.extend_from_slice(&0x0409u16.to_be_bytes()); // languageID
    t.extend_from_slice(&6u16.to_be_bytes()); // nameID (PostScript)
    t.extend_from_slice(&(string_data.len() as u16).to_be_bytes()); // length
    t.extend_from_slice(&0u16.to_be_bytes()); // offset
    t.extend_from_slice(&string_data);
    t
}

fn build_cmap(pairs: &[(u32, u16)]) -> Vec<u8> {
    // Format 4 with one segment per code point + the mandatory 0xFFFF terminator.
    let mut ends: Vec<u16> = Vec::new();
    let mut starts: Vec<u16> = Vec::new();
    let mut deltas: Vec<i16> = Vec::new();
    for &(cp, gid) in pairs {
        let c = cp as u16;
        ends.push(c);
        starts.push(c);
        deltas.push((gid as i32 - c as i32) as i16);
    }
    ends.push(0xFFFF);
    starts.push(0xFFFF);
    deltas.push(1);

    let seg_count = ends.len() as u16;
    let seg_x2 = seg_count * 2;
    let max_pow2 = 15u16 - seg_count.leading_zeros() as u16;
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
    for &e in &ends {
        sub.extend_from_slice(&e.to_be_bytes());
    }
    sub.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
    for &s in &starts {
        sub.extend_from_slice(&s.to_be_bytes());
    }
    for &d in &deltas {
        sub.extend_from_slice(&d.to_be_bytes());
    }
    for _ in 0..seg_count {
        sub.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset (all 0 → use delta)
    }
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

/// Latin-1 / special Standard Strings (SID 96..=228) → Unicode. Filled in
/// fragment chunks below to keep each table small.
fn latin1_sid_unicode(sid: u16) -> Option<u32> {
    LATIN1_CHUNK_LIST
        .iter()
        .flat_map(|chunk| chunk.iter())
        .find(|&&(s, _)| s == sid)
        .map(|&(_, u)| u)
}

/// (SID, Unicode) pairs for the Latin-1/special predefined Standard Strings
/// (SID 96..=228), split into chunks so each edit stays small.
const LATIN1_CHUNK_LIST: &[&[(u16, u32)]] = &[L1_A, L1_B, L1_C, L1_D];
const L1_A: &[(u16, u32)] = &[
    (96, 0xA1), (97, 0xA2), (98, 0xA3), (99, 0x2044), (100, 0xA5), (101, 0x192),
    (102, 0xA7), (103, 0xA4), (104, 0x27), (105, 0x201C), (106, 0xAB), (107, 0x2039),
    (108, 0x203A), (109, 0xFB01), (110, 0xFB02), (111, 0x2013), (112, 0x2020), (113, 0x2021),
    (114, 0xB7), (115, 0xB6), (116, 0x2022), (117, 0x201A), (118, 0x201E), (119, 0x201D),
    (120, 0xBB), (121, 0x2026), (122, 0x2030), (123, 0xBF), (124, 0x60), (125, 0xB4),
    (126, 0x2C6), (127, 0x2DC),
];
const L1_B: &[(u16, u32)] = &[
    (128, 0xAF), (129, 0x2D8), (130, 0x2D9), (131, 0xA8), (132, 0x2DA), (133, 0xB8),
    (134, 0x2DD), (135, 0x2DB), (136, 0x2C7), (137, 0x2014), (138, 0xC6), (139, 0xAA),
    (140, 0x141), (141, 0xD8), (142, 0x152), (143, 0xBA), (144, 0xE6), (145, 0x131),
    (146, 0x142), (147, 0xF8), (148, 0x153), (149, 0xDF), (150, 0xB9), (151, 0xAC),
    (152, 0xB5), (153, 0x2122), (154, 0xD0), (155, 0xBD), (156, 0xB1), (157, 0xDE),
    (158, 0xBC), (159, 0xF7), (160, 0xA6), (161, 0xB0), (162, 0xFE), (163, 0xBE),
    (164, 0xB2), (165, 0xAE), (166, 0x2212), (167, 0xF0), (168, 0xD7), (169, 0xB3),
    (170, 0xA9),
];
const L1_C: &[(u16, u32)] = &[
    (171, 0xC1), (172, 0xC2), (173, 0xC4), (174, 0xC0), (175, 0xC5), (176, 0xC3),
    (177, 0xC7), (178, 0xC9), (179, 0xCA), (180, 0xCB), (181, 0xC8), (182, 0xCD),
    (183, 0xCE), (184, 0xCF), (185, 0xCC), (186, 0xD1), (187, 0xD3), (188, 0xD4),
    (189, 0xD6), (190, 0xD2), (191, 0xD5), (192, 0x160), (193, 0xDA), (194, 0xDB),
    (195, 0xDC), (196, 0xD9), (197, 0xDD), (198, 0x178), (199, 0x17D),
];
const L1_D: &[(u16, u32)] = &[
    (200, 0xE1), (201, 0xE2), (202, 0xE4), (203, 0xE0), (204, 0xE5), (205, 0xE3),
    (206, 0xE7), (207, 0xE9), (208, 0xEA), (209, 0xEB), (210, 0xE8), (211, 0xED),
    (212, 0xEE), (213, 0xEF), (214, 0xEC), (215, 0xF1), (216, 0xF3), (217, 0xF4),
    (218, 0xF6), (219, 0xF2), (220, 0xF5), (221, 0x161), (222, 0xFA), (223, 0xFB),
    (224, 0xFC), (225, 0xF9), (226, 0xFD), (227, 0xFF), (228, 0x17E),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_name_unicode_conventions() {
        assert_eq!(glyph_name_to_unicode("uni00E9"), Some(0xE9)); // é
        assert_eq!(glyph_name_to_unicode("u1F600"), Some(0x1F600)); // emoji
        assert_eq!(glyph_name_to_unicode("A"), Some(0x41)); // single char
        assert_eq!(glyph_name_to_unicode("ABCDEF+A"), Some(0x41)); // subset prefix stripped
        assert_eq!(glyph_name_to_unicode("a.sc"), Some(0x61)); // suffix stripped
        assert_eq!(glyph_name_to_unicode("ffi"), None); // ligature: no base scalar
        assert_eq!(glyph_name_to_unicode(".notdef"), None);
    }

    #[test]
    fn latin1_standard_strings() {
        assert_eq!(latin1_sid_unicode(96), Some(0xA1)); // exclamdown
        assert_eq!(latin1_sid_unicode(207), Some(0xE9)); // eacute → é (French)
        assert_eq!(latin1_sid_unicode(203), Some(0xE0)); // agrave → à
        assert_eq!(latin1_sid_unicode(206), Some(0xE7)); // ccedilla → ç
        assert_eq!(latin1_sid_unicode(228), Some(0x17E)); // zcaron
        assert_eq!(latin1_sid_unicode(95), None); // below the Latin-1 range
        assert_eq!(latin1_sid_unicode(229), None); // stylistic, no code point
    }

    #[test]
    fn synthesised_sfnt_parses_with_a_working_cmap() {
        // Assemble an OTTO exactly as `wrap` does (stub CFF table — the metric
        // reader only consumes cmap/hmtx/head/maxp), then verify the engine's
        // own reader accepts it and the synthesised cmap resolves code points.
        let pairs = [(0x41u32, 3u16), (0xE9u32, 7u16)]; // 'A' → gid 3, 'é' → gid 7
        let advances = vec![500u16; 8];
        let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"CFF ", vec![1, 0, 4, 1]),
            (b"OS/2", build_os2(500, 8)),
            (b"cmap", build_cmap(&pairs)),
            (b"head", build_head(1000)),
            (b"hhea", build_hhea(500, 8)),
            (b"hmtx", build_hmtx(&advances)),
            (b"maxp", build_maxp(8)),
            (b"name", build_name()),
            (b"post", build_post()),
        ];
        tables.sort_by(|a, b| a.0.cmp(b.0));
        let otf = assemble_sfnt(0x4F54_544F, &mut tables);

        assert_eq!(&otf[0..4], b"OTTO");
        let parsed = crate::font::truetype::TrueTypeFont::parse_metrics(&otf)
            .expect("synthesised OTTO must parse");
        assert_eq!(parsed.num_glyphs(), 8);
        assert_eq!(parsed.gid_for_unicode(0x41), Some(3));
        assert_eq!(parsed.gid_for_unicode(0xE9), Some(7));
        assert_eq!(parsed.gid_for_unicode(0x2603), None); // unmapped
    }

    #[test]
    fn wrap_rejects_non_cff() {
        assert!(wrap(b"not a font").is_none());
    }
}
