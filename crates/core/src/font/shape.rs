//! Minimal OpenType-Layout shaper (`GSUB` + `GPOS`) — zero dependencies.
//!
//! Enough of the OpenType layout model to position Latin text faithfully when
//! measuring and laying out HTML: **GSUB** standard ligatures (`liga`/`ccmp`)
//! and single substitutions, and **GPOS** pair kerning (`kern`). Everything is
//! parsed directly from the font's `GSUB`/`GPOS` table bytes (exposed by
//! [`TrueTypeFont`]); no new tokenizer, no allocation of the whole table.
//!
//! The shaper turns a Unicode string into a sequence of `(glyph id, advance)`
//! where the advance already folds in the font's base `hmtx` width plus any
//! GPOS pair-kerning adjustment, and where consecutive glyphs may have been
//! merged into a single ligature glyph by GSUB. Used by the HTML measurer/
//! painter so kerned/ligated text lays out where a real shaper would put it.
//!
//! Scope (assumed, documented limits): default script / default language only;
//! GSUB lookup types 1 (single), 4 (ligature) and 7 (extension wrapping those);
//! GPOS lookup types 2 (pair adjustment, formats 1 & 2) and 9 (extension). Mark
//! positioning, contextual chaining and the rest resolve to no-ops — which is
//! correct for ordinary office/document Latin text.

use super::truetype::TrueTypeFont;

fn be16(d: &[u8], o: usize) -> u16 {
    if o + 2 <= d.len() {
        ((d[o] as u16) << 8) | d[o + 1] as u16
    } else {
        0
    }
}

fn bei16(d: &[u8], o: usize) -> i16 {
    be16(d, o) as i16
}

fn be32(d: &[u8], o: usize) -> u32 {
    if o + 4 <= d.len() {
        ((d[o] as u32) << 24)
            | ((d[o + 1] as u32) << 16)
            | ((d[o + 2] as u32) << 8)
            | d[o + 3] as u32
    } else {
        0
    }
}

/// One ligature rule: a tail of component glyph ids that, when they follow the
/// first component, are replaced by a single `ligature` glyph.
#[derive(Debug, Clone)]
struct Ligature {
    ligature: u16,
    components_tail: Vec<u16>,
}

/// A parsed shaper for one font: the GSUB substitution rules and the GPOS pair
/// kerning, ready to apply to a glyph run.
#[derive(Debug, Clone, Default)]
pub struct Shaper {
    /// `from_gid → to_gid` single substitutions (GSUB type 1).
    single: std::collections::BTreeMap<u16, u16>,
    /// `first_gid → ligature rules` (GSUB type 4), longest tail tried first.
    ligatures: std::collections::BTreeMap<u16, Vec<Ligature>>,
    /// `(left_gid, right_gid) → x-advance adjustment` in font units (GPOS type 2).
    kern: std::collections::BTreeMap<(u16, u16), i32>,
}

impl Shaper {
    /// Build the shaper for a font by parsing its `GSUB` and `GPOS` tables.
    /// Returns an empty (no-op) shaper when neither table is present, so callers
    /// can always shape without branching.
    pub fn new(ttf: &TrueTypeFont) -> Shaper {
        let mut s = Shaper::default();
        let data = ttf.data();
        if let Some((off, len)) = ttf.gsub_range() {
            if off + len <= data.len() {
                s.parse_gsub(data, off);
            }
        }
        if let Some((off, len)) = ttf.gpos_range() {
            if off + len <= data.len() {
                s.parse_gpos(data, off);
            }
        }
        s
    }

    /// Whether this shaper carries no rules at all (no substitutions, no kerning)
    /// — the caller can then skip shaping entirely.
    pub fn is_empty(&self) -> bool {
        self.single.is_empty() && self.ligatures.is_empty() && self.kern.is_empty()
    }

    /// Apply GSUB single + ligature substitutions to a glyph-id run, returning
    /// the substituted glyph ids. (Used before measuring so the advance sum is
    /// taken over the *shaped* glyphs.)
    pub fn substitute(&self, gids: &[u16]) -> Vec<u16> {
        let mut out = Vec::with_capacity(gids.len());
        let mut i = 0;
        while i < gids.len() {
            // Ligatures first (they consume several inputs); fall back to single.
            if let Some((lig, consumed)) = self.match_ligature(&gids[i..]) {
                out.push(lig);
                i += consumed;
                continue;
            }
            let g = gids[i];
            out.push(self.single.get(&g).copied().unwrap_or(g));
            i += 1;
        }
        out
    }

    /// The GPOS pair-kern x-advance adjustment (font units) between two adjacent
    /// glyphs, or `0` when the pair is not kerned.
    pub fn kern(&self, left: u16, right: u16) -> i32 {
        self.kern.get(&(left, right)).copied().unwrap_or(0)
    }

    /// Every ligature substitution as `(ligature_gid, [component_gids…])` (the
    /// full component list, first glyph included). Lets the embedder map a
    /// ligature glyph back to its component Unicode for `/ToUnicode`, so a
    /// ligated run still extracts/copies as the original characters.
    pub fn ligature_rules(&self) -> Vec<(u16, Vec<u16>)> {
        let mut out = Vec::new();
        for (&first, rules) in &self.ligatures {
            for rule in rules {
                let mut comps = Vec::with_capacity(rule.components_tail.len() + 1);
                comps.push(first);
                comps.extend_from_slice(&rule.components_tail);
                out.push((rule.ligature, comps));
            }
        }
        out
    }

    /// Longest ligature whose components match the head of `gids` starting at the
    /// first glyph. Returns `(ligature_gid, glyphs_consumed)`.
    fn match_ligature(&self, gids: &[u16]) -> Option<(u16, usize)> {
        let first = *gids.first()?;
        let rules = self.ligatures.get(&first)?;
        // Rules are pre-sorted longest-tail-first so the greediest match wins.
        for rule in rules {
            let tail = &rule.components_tail;
            if tail.len() < gids.len() && gids[1..=tail.len()] == tail[..] {
                return Some((rule.ligature, tail.len() + 1));
            }
        }
        None
    }

    // ── GSUB ────────────────────────────────────────────────────────────────

    fn parse_gsub(&mut self, d: &[u8], base: usize) {
        let lookups = match self.layout_lookups(d, base, &[*b"liga", *b"ccmp", *b"clig", *b"rlig"])
        {
            Some(v) => v,
            None => return,
        };
        for lookup_off in lookups {
            self.parse_gsub_lookup(d, lookup_off, 0);
        }
        // Sort each glyph's ligature rules longest-tail-first (greedy matching).
        for rules in self.ligatures.values_mut() {
            rules.sort_by(|a, b| b.components_tail.len().cmp(&a.components_tail.len()));
        }
    }

    fn parse_gsub_lookup(&mut self, d: &[u8], lookup_off: usize, depth: u8) {
        if depth > 4 {
            return; // guard pathological extension chains
        }
        let lookup_type = be16(d, lookup_off);
        let subtable_count = be16(d, lookup_off + 4) as usize;
        for i in 0..subtable_count {
            let sub = lookup_off + 6 + i * 2;
            let sub_off = lookup_off + be16(d, sub) as usize;
            match lookup_type {
                1 => self.parse_single_subst(d, sub_off),
                4 => self.parse_ligature_subst(d, sub_off),
                7 => {
                    // Extension: format 1, real type at +2, 32-bit offset at +4.
                    if be16(d, sub_off) == 1 {
                        let real_type = be16(d, sub_off + 2);
                        let real_off = sub_off + be32(d, sub_off + 4) as usize;
                        match real_type {
                            1 => self.parse_single_subst(d, real_off),
                            4 => self.parse_ligature_subst(d, real_off),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn parse_single_subst(&mut self, d: &[u8], off: usize) {
        let format = be16(d, off);
        let coverage_off = off + be16(d, off + 2) as usize;
        let covered = parse_coverage(d, coverage_off);
        match format {
            1 => {
                let delta = bei16(d, off + 4) as i32;
                for g in covered {
                    let to = (g as i32 + delta) as u16;
                    self.single.entry(g).or_insert(to);
                }
            }
            2 => {
                let count = be16(d, off + 4) as usize;
                for (idx, g) in covered.into_iter().enumerate() {
                    if idx < count {
                        let to = be16(d, off + 6 + idx * 2);
                        self.single.entry(g).or_insert(to);
                    }
                }
            }
            _ => {}
        }
    }

    fn parse_ligature_subst(&mut self, d: &[u8], off: usize) {
        if be16(d, off) != 1 {
            return; // only format 1 exists
        }
        let coverage_off = off + be16(d, off + 2) as usize;
        let covered = parse_coverage(d, coverage_off);
        let set_count = be16(d, off + 4) as usize;
        for (idx, first_gid) in covered.into_iter().enumerate() {
            if idx >= set_count {
                break;
            }
            let set_off = off + be16(d, off + 6 + idx * 2) as usize;
            let lig_count = be16(d, set_off) as usize;
            for j in 0..lig_count {
                let lig_off = set_off + be16(d, set_off + 2 + j * 2) as usize;
                let ligature = be16(d, lig_off);
                let comp_count = be16(d, lig_off + 2) as usize; // includes first
                if comp_count == 0 {
                    continue;
                }
                // Stored component glyphs are the *tail* (component[1..]).
                let mut tail = Vec::with_capacity(comp_count.saturating_sub(1));
                for k in 1..comp_count {
                    tail.push(be16(d, lig_off + 4 + (k - 1) * 2));
                }
                self.ligatures.entry(first_gid).or_default().push(Ligature {
                    ligature,
                    components_tail: tail,
                });
            }
        }
    }

    // ── GPOS ────────────────────────────────────────────────────────────────

    fn parse_gpos(&mut self, d: &[u8], base: usize) {
        let lookups = match self.layout_lookups(d, base, &[*b"kern"]) {
            Some(v) => v,
            None => return,
        };
        for lookup_off in lookups {
            self.parse_gpos_lookup(d, lookup_off, 0);
        }
    }

    fn parse_gpos_lookup(&mut self, d: &[u8], lookup_off: usize, depth: u8) {
        if depth > 4 {
            return;
        }
        let lookup_type = be16(d, lookup_off);
        let subtable_count = be16(d, lookup_off + 4) as usize;
        for i in 0..subtable_count {
            let sub_off = lookup_off + be16(d, lookup_off + 6 + i * 2) as usize;
            match lookup_type {
                2 => self.parse_pair_pos(d, sub_off),
                9 => {
                    if be16(d, sub_off) == 1 && be16(d, sub_off + 2) == 2 {
                        let real_off = sub_off + be32(d, sub_off + 4) as usize;
                        self.parse_pair_pos(d, real_off);
                    }
                }
                _ => {}
            }
        }
    }

    fn parse_pair_pos(&mut self, d: &[u8], off: usize) {
        let format = be16(d, off);
        let coverage_off = off + be16(d, off + 2) as usize;
        let value_format1 = be16(d, off + 4);
        let value_format2 = be16(d, off + 6);
        let v1_size = value_record_size(value_format1);
        let v2_size = value_record_size(value_format2);
        // Only the XAdvance of the first glyph matters for horizontal kerning.
        let v1_xadv = value_has_xadvance(value_format1);
        match format {
            1 => {
                let covered = parse_coverage(d, coverage_off);
                let pair_set_count = be16(d, off + 8) as usize;
                for (idx, left) in covered.into_iter().enumerate() {
                    if idx >= pair_set_count {
                        break;
                    }
                    let set_off = off + be16(d, off + 10 + idx * 2) as usize;
                    let pair_count = be16(d, set_off) as usize;
                    let record_size = 2 + v1_size + v2_size;
                    for j in 0..pair_count {
                        let rec = set_off + 2 + j * record_size;
                        let right = be16(d, rec);
                        if v1_xadv {
                            let adj = bei16(d, rec + 2) as i32; // XAdvance is first field
                            if adj != 0 {
                                self.kern.entry((left, right)).or_insert(adj);
                            }
                        }
                    }
                }
            }
            2 => {
                let class_def1 = off + be16(d, off + 8) as usize;
                let class_def2 = off + be16(d, off + 10) as usize;
                let class1_count = be16(d, off + 12) as usize;
                let class2_count = be16(d, off + 14) as usize;
                let record_size = v1_size + v2_size;
                if !v1_xadv || class1_count == 0 || class2_count == 0 {
                    return;
                }
                // Build class → glyph-id lists so the (left,right) pairs can be
                // materialised. Glyphs outside any class are class 0.
                let covered = parse_coverage(d, coverage_off);
                let classes1 = parse_class_def(d, class_def1);
                let classes2 = parse_class_def(d, class_def2);
                // Only enumerate pairs whose left glyph is in the coverage (the
                // glyphs this subtable actually positions) to keep it bounded.
                let base_rec = off + 16;
                for &left in &covered {
                    let c1 = classes1.get(&left).copied().unwrap_or(0) as usize;
                    if c1 >= class1_count {
                        continue;
                    }
                    for &right in classes2.keys() {
                        let c2 = classes2.get(&right).copied().unwrap_or(0) as usize;
                        if c2 >= class2_count {
                            continue;
                        }
                        let rec = base_rec + (c1 * class2_count + c2) * record_size;
                        let adj = bei16(d, rec) as i32; // XAdvance is first field
                        if adj != 0 {
                            self.kern.entry((left, right)).or_insert(adj);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ── shared script/feature/lookup resolution ──────────────────────────────

    /// Resolve the lookup-table file offsets for the given feature tags under the
    /// default script's default language system (GSUB/GPOS share this layout).
    /// Returns `None` when the table has no usable script/feature/lookup lists.
    fn layout_lookups(&self, d: &[u8], base: usize, wanted: &[[u8; 4]]) -> Option<Vec<usize>> {
        let script_list = base + be16(d, base + 4) as usize;
        let feature_list = base + be16(d, base + 6) as usize;
        let lookup_list = base + be16(d, base + 8) as usize;
        if be16(d, base + 8) == 0 {
            return None; // no LookupList
        }

        // Pick the default langsys: prefer DFLT script, else the first script.
        let lang_sys = self.default_lang_sys(d, script_list)?;
        let feature_index_count = be16(d, lang_sys + 4) as usize;

        // Collect feature indices whose tag is in `wanted`.
        let feature_count = be16(d, feature_list) as usize;
        let mut lookup_indices: Vec<u16> = Vec::new();
        for i in 0..feature_index_count {
            let fi = be16(d, lang_sys + 6 + i * 2) as usize;
            if fi >= feature_count {
                continue;
            }
            let rec = feature_list + 2 + fi * 6;
            let mut tag = [0u8; 4];
            if rec + 4 <= d.len() {
                tag.copy_from_slice(&d[rec..rec + 4]);
            }
            if !wanted.contains(&tag) {
                continue;
            }
            let feature_off = feature_list + be16(d, rec + 4) as usize;
            let n = be16(d, feature_off + 2) as usize;
            for j in 0..n {
                lookup_indices.push(be16(d, feature_off + 4 + j * 2));
            }
        }
        lookup_indices.sort_unstable();
        lookup_indices.dedup();

        // Map lookup indices → lookup-table offsets.
        let lookup_count = be16(d, lookup_list) as usize;
        let mut out = Vec::with_capacity(lookup_indices.len());
        for li in lookup_indices {
            let li = li as usize;
            if li < lookup_count {
                out.push(lookup_list + be16(d, lookup_list + 2 + li * 2) as usize);
            }
        }
        Some(out)
    }

    /// The default LanguageSystem table offset: the `DFLT` script's default
    /// langsys if present, otherwise the first script's default langsys.
    fn default_lang_sys(&self, d: &[u8], script_list: usize) -> Option<usize> {
        let script_count = be16(d, script_list) as usize;
        let mut first: Option<usize> = None;
        for i in 0..script_count {
            let rec = script_list + 2 + i * 6;
            let mut tag = [0u8; 4];
            if rec + 4 <= d.len() {
                tag.copy_from_slice(&d[rec..rec + 4]);
            }
            let script_off = script_list + be16(d, rec + 4) as usize;
            let default_lang = be16(d, script_off);
            if default_lang == 0 {
                continue; // this script has no default langsys
            }
            let lang_off = script_off + default_lang as usize;
            if &tag == b"DFLT" {
                return Some(lang_off);
            }
            first.get_or_insert(lang_off);
        }
        first
    }
}

/// Parse a Coverage table (format 1 list or format 2 ranges) into the covered
/// glyph ids, **in coverage index order** (so index → glyph correspondence is
/// preserved for format-2 single substitutions and pair-set indexing).
fn parse_coverage(d: &[u8], off: usize) -> Vec<u16> {
    let mut out = Vec::new();
    match be16(d, off) {
        1 => {
            let count = be16(d, off + 2) as usize;
            for i in 0..count {
                out.push(be16(d, off + 4 + i * 2));
            }
        }
        2 => {
            let range_count = be16(d, off + 2) as usize;
            for i in 0..range_count {
                let r = off + 4 + i * 6;
                let start = be16(d, r);
                let end = be16(d, r + 2);
                if end >= start {
                    // `end`/`start` are u16, so the span is already bounded.
                    let span = end - start;
                    for g in 0..=span {
                        out.push(start + g);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// Parse a ClassDef table (format 1 or 2) into a `glyph id → class` map. Glyphs
/// absent from the map are class 0 by definition.
fn parse_class_def(d: &[u8], off: usize) -> std::collections::BTreeMap<u16, u16> {
    let mut map = std::collections::BTreeMap::new();
    match be16(d, off) {
        1 => {
            let start = be16(d, off + 2);
            let count = be16(d, off + 4) as usize;
            for i in 0..count {
                let class = be16(d, off + 6 + i * 2);
                if class != 0 {
                    map.insert(start.wrapping_add(i as u16), class);
                }
            }
        }
        2 => {
            let range_count = be16(d, off + 2) as usize;
            for i in 0..range_count {
                let r = off + 4 + i * 6;
                let start = be16(d, r);
                let end = be16(d, r + 2);
                let class = be16(d, r + 4);
                if class != 0 && end >= start {
                    // `end`/`start` are u16, so the span is already bounded.
                    let span = end - start;
                    for g in 0..=span {
                        map.insert(start + g, class);
                    }
                }
            }
        }
        _ => {}
    }
    map
}

/// Number of bytes in a GPOS ValueRecord with the given ValueFormat flags (each
/// set bit = one i16 field).
fn value_record_size(format: u16) -> usize {
    (format.count_ones() as usize) * 2
}

/// Whether a ValueFormat includes an XAdvance field (bit 0x0004).
fn value_has_xadvance(format: u16) -> bool {
    format & 0x0004 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a tiny GSUB table with one ligature substitution (gids A,B → L) and
    // wrap it in a minimal OpenType font so the public Shaper path is exercised.
    // We assemble GSUB bytes by hand following ISO/IEC 14496-22.
    fn gsub_with_liga(first: u16, second: u16, lig: u16) -> Vec<u8> {
        // Layout (offsets relative to GSUB start):
        //   0  : header (version 1.0, scriptListOff, featureListOff, lookupListOff)
        //   10 : ScriptList
        //   ?? : FeatureList
        //   ?? : LookupList → Lookup(type4) → LigatureSubst → LigatureSet → Ligature
        let mut b = Vec::new();
        // We compute section offsets up front.
        let script_list_off = 10u16;
        // ScriptList: count=1, DFLT → script table.
        // ScriptRecord = 6 bytes; script table starts right after.
        let script_table_off = script_list_off + 2 + 6;
        // Script table: defaultLangSys offset (2) + langSysCount (2) = 4 bytes,
        // then LangSys.
        let langsys_off = script_table_off + 4;
        // LangSys: lookupOrderOff(2)=0, reqFeatureIndex(2)=0xFFFF, featureCount(2)=1,
        // featureIndices[1](2) = 8 bytes.
        let feature_list_off = langsys_off + 8;
        // FeatureList: featureCount(2)=1 + FeatureRecord(6) = 8, then Feature.
        let feature_off = feature_list_off + 8;
        // Feature: featureParams(2)=0, lookupIndexCount(2)=1, lookupIndices[1](2)=6.
        let lookup_list_off = feature_off + 6;
        // LookupList: lookupCount(2)=1 + lookupOffsets[1](2) = 4, then Lookup.
        let lookup_off = lookup_list_off + 4;
        // Lookup: type(2)=4, flag(2)=0, subtableCount(2)=1, subtableOffsets[1](2)=8.
        let ligsubst_off = lookup_off + 8;
        // LigatureSubst: format(2)=1, coverageOff(2), ligSetCount(2)=1,
        // ligatureSetOffsets[1](2) = 8 bytes, then Coverage + LigatureSet + Ligature.
        let coverage_off = ligsubst_off + 8;
        // Coverage format1: format(2)=1, glyphCount(2)=1, glyphArray[1](2) = 6 bytes.
        let ligset_off = coverage_off + 6;
        // LigatureSet: ligatureCount(2)=1, ligatureOffsets[1](2) = 4 bytes, then Ligature.
        let ligature_off = ligset_off + 4;
        // Ligature: ligatureGlyph(2), componentCount(2)=2, componentGlyphIDs[1](2) = 6 bytes.

        // header
        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b.extend_from_slice(&script_list_off.to_be_bytes());
        b.extend_from_slice(&feature_list_off.to_be_bytes());
        b.extend_from_slice(&lookup_list_off.to_be_bytes());
        debug_assert_eq!(b.len(), script_list_off as usize);
        // ScriptList
        b.extend_from_slice(&1u16.to_be_bytes()); // scriptCount
        b.extend_from_slice(b"DFLT"); // scriptTag
        b.extend_from_slice(&(script_table_off - script_list_off).to_be_bytes());
        debug_assert_eq!(b.len(), script_table_off as usize);
        // Script table
        b.extend_from_slice(&(langsys_off - script_table_off).to_be_bytes()); // defaultLangSys
        b.extend_from_slice(&0u16.to_be_bytes()); // langSysCount
        debug_assert_eq!(b.len(), langsys_off as usize);
        // LangSys
        b.extend_from_slice(&0u16.to_be_bytes()); // lookupOrder
        b.extend_from_slice(&0xFFFFu16.to_be_bytes()); // requiredFeatureIndex
        b.extend_from_slice(&1u16.to_be_bytes()); // featureIndexCount
        b.extend_from_slice(&0u16.to_be_bytes()); // featureIndices[0]
        debug_assert_eq!(b.len(), feature_list_off as usize);
        // FeatureList
        b.extend_from_slice(&1u16.to_be_bytes()); // featureCount
        b.extend_from_slice(b"liga"); // featureTag
        b.extend_from_slice(&(feature_off - feature_list_off).to_be_bytes());
        debug_assert_eq!(b.len(), feature_off as usize);
        // Feature
        b.extend_from_slice(&0u16.to_be_bytes()); // featureParams
        b.extend_from_slice(&1u16.to_be_bytes()); // lookupIndexCount
        b.extend_from_slice(&0u16.to_be_bytes()); // lookupListIndices[0]
        debug_assert_eq!(b.len(), lookup_list_off as usize);
        // LookupList
        b.extend_from_slice(&1u16.to_be_bytes()); // lookupCount
        b.extend_from_slice(&(lookup_off - lookup_list_off).to_be_bytes());
        debug_assert_eq!(b.len(), lookup_off as usize);
        // Lookup
        b.extend_from_slice(&4u16.to_be_bytes()); // lookupType (ligature)
        b.extend_from_slice(&0u16.to_be_bytes()); // lookupFlag
        b.extend_from_slice(&1u16.to_be_bytes()); // subTableCount
        b.extend_from_slice(&(ligsubst_off - lookup_off).to_be_bytes());
        debug_assert_eq!(b.len(), ligsubst_off as usize);
        // LigatureSubst
        b.extend_from_slice(&1u16.to_be_bytes()); // substFormat
        b.extend_from_slice(&(coverage_off - ligsubst_off).to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes()); // ligatureSetCount
        b.extend_from_slice(&(ligset_off - ligsubst_off).to_be_bytes());
        debug_assert_eq!(b.len(), coverage_off as usize);
        // Coverage (format 1)
        b.extend_from_slice(&1u16.to_be_bytes()); // coverageFormat
        b.extend_from_slice(&1u16.to_be_bytes()); // glyphCount
        b.extend_from_slice(&first.to_be_bytes()); // glyphArray[0]
        debug_assert_eq!(b.len(), ligset_off as usize);
        // LigatureSet
        b.extend_from_slice(&1u16.to_be_bytes()); // ligatureCount
        b.extend_from_slice(&(ligature_off - ligset_off).to_be_bytes());
        debug_assert_eq!(b.len(), ligature_off as usize);
        // Ligature
        b.extend_from_slice(&lig.to_be_bytes()); // ligatureGlyph
        b.extend_from_slice(&2u16.to_be_bytes()); // componentCount
        b.extend_from_slice(&second.to_be_bytes()); // componentGlyphIDs[0] (tail)
        b
    }

    // GPOS with a single PairPos format-1 subtable: (left,right) → xAdvance adj.
    fn gpos_with_kern(left: u16, right: u16, adj: i16) -> Vec<u8> {
        let mut b = Vec::new();
        let script_list_off = 10u16;
        let script_table_off = script_list_off + 2 + 6;
        let langsys_off = script_table_off + 4;
        // LangSys is 8 bytes: lookupOrder(2) + reqFeatureIndex(2)
        // + featureIndexCount(2) + featureIndices[1](2).
        let feature_list_off = langsys_off + 8;
        let feature_off = feature_list_off + 8;
        let lookup_list_off = feature_off + 6;
        let lookup_off = lookup_list_off + 4;
        let pairpos_off = lookup_off + 8;
        // PairPos format1: format(2), coverageOff(2), valueFormat1(2),
        // valueFormat2(2), pairSetCount(2), pairSetOffsets[1](2) = 12 bytes.
        let coverage_off = pairpos_off + 12;
        // Coverage format1: 6 bytes.
        let pairset_off = coverage_off + 6;

        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b.extend_from_slice(&script_list_off.to_be_bytes());
        b.extend_from_slice(&feature_list_off.to_be_bytes());
        b.extend_from_slice(&lookup_list_off.to_be_bytes());
        // ScriptList
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(b"DFLT");
        b.extend_from_slice(&(script_table_off - script_list_off).to_be_bytes());
        // Script
        b.extend_from_slice(&(langsys_off - script_table_off).to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // LangSys
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0xFFFFu16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // FeatureList
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(b"kern");
        b.extend_from_slice(&(feature_off - feature_list_off).to_be_bytes());
        // Feature
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // LookupList
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(lookup_off - lookup_list_off).to_be_bytes());
        // Lookup
        b.extend_from_slice(&2u16.to_be_bytes()); // lookupType (pair adjustment)
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(pairpos_off - lookup_off).to_be_bytes());
        // PairPos format1
        b.extend_from_slice(&1u16.to_be_bytes()); // posFormat
        b.extend_from_slice(&(coverage_off - pairpos_off).to_be_bytes());
        b.extend_from_slice(&0x0004u16.to_be_bytes()); // valueFormat1 = XAdvance
        b.extend_from_slice(&0u16.to_be_bytes()); // valueFormat2 = none
        b.extend_from_slice(&1u16.to_be_bytes()); // pairSetCount
        b.extend_from_slice(&(pairset_off - pairpos_off).to_be_bytes());
        // Coverage (format 1)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&left.to_be_bytes());
        // PairSet
        b.extend_from_slice(&1u16.to_be_bytes()); // pairValueCount
        b.extend_from_slice(&right.to_be_bytes()); // secondGlyph
        b.extend_from_slice(&adj.to_be_bytes()); // value1.xAdvance
        b
    }

    // Wrap raw GSUB/GPOS bytes into a minimal sfnt so TrueTypeFont::parse_metrics
    // accepts it and exposes the table ranges to the Shaper. Reuses the OTTO
    // assembler (we only need head/hhea/maxp + the layout table).
    fn font_with_layout(tag: &[u8; 4], table: Vec<u8>) -> Vec<u8> {
        use crate::font::cff_to_otf::assemble_sfnt;
        // Minimal required tables for parse_metrics: head, hhea, maxp + ours.
        let mut head = vec![0u8; 54];
        head[18] = 0x03;
        head[19] = 0xE8; // unitsPerEm = 1000
        let mut hhea = vec![0u8; 36];
        hhea[34] = 0x00;
        hhea[35] = 0x10; // numberOfHMetrics = 16
        let mut maxp = vec![0u8; 6];
        maxp[0] = 0x00;
        maxp[1] = 0x00;
        maxp[2] = 0x50;
        maxp[3] = 0x00; // version 0.5
        maxp[4] = 0x01;
        maxp[5] = 0x00; // numGlyphs = 256
        let mut hmtx = Vec::new();
        for _ in 0..16 {
            hmtx.extend_from_slice(&500u16.to_be_bytes());
            hmtx.extend_from_slice(&0i16.to_be_bytes());
        }
        let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"head", head),
            (b"hhea", hhea),
            (b"maxp", maxp),
            (b"hmtx", hmtx),
            (tag, table),
        ];
        tables.sort_by(|a, b| a.0.cmp(b.0));
        assemble_sfnt(0x4F54_544F, &mut tables)
    }

    #[test]
    fn gsub_ligature_substitution_applies() {
        // gids 10,11 → ligature 99.
        let gsub = gsub_with_liga(10, 11, 99);
        let font = font_with_layout(b"GSUB", gsub);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let shaper = Shaper::new(&ttf);
        assert!(!shaper.is_empty(), "shaper picked up the GSUB rule");
        // The pair 10,11 collapses to the single ligature glyph 99.
        assert_eq!(shaper.substitute(&[10, 11]), vec![99]);
        // A lone 10 (no following 11) is untouched.
        assert_eq!(shaper.substitute(&[10, 12]), vec![10, 12]);
        // 10,11 in the middle of a run still ligates, surroundings preserved.
        assert_eq!(shaper.substitute(&[5, 10, 11, 7]), vec![5, 99, 7]);
    }

    #[test]
    fn gpos_pair_kerning_is_negative_for_close_pair() {
        // Kern (left=20, right=21) by -80 font units (typical "AV" tightening).
        let gpos = gpos_with_kern(20, 21, -80);
        let font = font_with_layout(b"GPOS", gpos);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let shaper = Shaper::new(&ttf);
        assert!(!shaper.is_empty(), "shaper picked up the GPOS rule");
        assert_eq!(shaper.kern(20, 21), -80);
        assert_eq!(shaper.kern(21, 20), 0, "unkerned pair → no adjustment");

        // The kerned advance of the pair is strictly less than the unkerned sum.
        let base = ttf.advance_width(20) + ttf.advance_width(21);
        let kerned = base + shaper.kern(20, 21) as f64;
        assert!(
            kerned < base,
            "kerned advance {kerned} < unkerned sum {base}"
        );
    }

    #[test]
    fn empty_shaper_for_plain_font() {
        // A font with no GSUB/GPOS yields a no-op shaper.
        let font = font_with_layout(b"post", vec![0u8; 32]);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let shaper = Shaper::new(&ttf);
        assert!(shaper.is_empty());
        assert_eq!(shaper.substitute(&[1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(shaper.kern(1, 2), 0);
    }
}
