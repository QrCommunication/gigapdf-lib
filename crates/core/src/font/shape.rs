//! OpenType-Layout shaper (`GSUB` + `GPOS`) — zero dependencies.
//!
//! Two layers live here, sharing the same table parsing:
//!
//! * **Latin fast path** (`substitute` + `kern`): GSUB standard ligatures
//!   (`liga`/`ccmp`/`clig`/`rlig`) and single substitutions, and GPOS pair
//!   kerning (`kern`), resolved for the **default script / default language**.
//!   It turns a glyph-id run into a (possibly ligated) glyph-id run plus a
//!   per-adjacent-pair x-advance adjustment, which the HTML measurer/painter
//!   folds into the `hmtx` advances. This path is unchanged and drives all
//!   ordinary office/document text.
//!
//! * **Complex path** ([`Shaper::shape`]): a positioned shaper that selects the
//!   right **script** (latn/arab/hebr…) and applies, in order, the script's
//!   substitution features — including **GSUB contextual / chaining contextual**
//!   (types 5/6) and, for cursive scripts, **Arabic joining** (`init`/`medi`/
//!   `fina`/`isol`) — then GPOS **pair kerning** (types 2/9) and **mark
//!   positioning**: mark-to-base (4), mark-to-ligature (5) and mark-to-mark (6).
//!   It returns [`ShapedGlyph`]s carrying per-glyph x/y placement offsets and
//!   advances, so diacritics sit on their base and cursive scripts render in
//!   contextual forms. Marks are identified via `GDEF`.
//!
//! Everything is parsed directly from the font's `GSUB`/`GPOS`/`GDEF` bytes
//! (exposed by [`TrueTypeFont`]); no new tokenizer, no allocation of the whole
//! table, and every offset read is bounds-checked (a malformed table degrades
//! to a no-op rather than panicking).
//!
//! Documented limits (assumed): full Indic/Khmer/Myanmar reordering (the
//! HarfBuzz-style syllable machine) is **out of scope** — only substitution,
//! joining and positioning that need no glyph reordering are performed. GSUB
//! reverse-chaining (type 8) and GPOS cursive attachment (type 3) are no-ops.

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
    /// `from_gid → to_gid` single substitutions (GSUB type 1), default script.
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
        let lookups = match self.layout_lookups(
            d,
            base,
            ScriptSelector::Default,
            &[*b"liga", *b"ccmp", *b"clig", *b"rlig"],
        ) {
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
        let lookups = match self.layout_lookups(d, base, ScriptSelector::Default, &[*b"kern"]) {
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

    /// Resolve the lookup-table file offsets for the given feature tags under a
    /// chosen script's default language system (GSUB/GPOS share this layout).
    /// Returns `None` when the table has no usable script/feature/lookup lists.
    fn layout_lookups(
        &self,
        d: &[u8],
        base: usize,
        selector: ScriptSelector,
        wanted: &[[u8; 4]],
    ) -> Option<Vec<usize>> {
        let script_list = base + be16(d, base + 4) as usize;
        let feature_list = base + be16(d, base + 6) as usize;
        let lookup_list = base + be16(d, base + 8) as usize;
        if be16(d, base + 8) == 0 {
            return None; // no LookupList
        }

        let lang_sys = self.select_lang_sys(d, script_list, selector)?;
        let feature_index_count = be16(d, lang_sys + 4) as usize;

        // Collect feature indices whose tag is in `wanted`, preserving the order
        // features are referenced in (the langsys order ≈ application order).
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
        // De-duplicate while keeping first-seen order: a lookup must run once,
        // and lookups apply in LookupList order anyway (sorted below) — but for
        // the default-script maps the previous behaviour sorted ascending, which
        // we preserve to keep the Latin path byte-for-byte identical.
        lookup_indices.sort_unstable();
        lookup_indices.dedup();

        // Map lookup indices → lookup-table offsets (in LookupList order).
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

    /// The LanguageSystem table offset for the requested script. `Default` keeps
    /// the historical behaviour (DFLT script, else the first script); a specific
    /// `Tag` matches that script and falls back to DFLT/first when absent.
    fn select_lang_sys(
        &self,
        d: &[u8],
        script_list: usize,
        selector: ScriptSelector,
    ) -> Option<usize> {
        let script_count = be16(d, script_list) as usize;
        let mut dflt: Option<usize> = None;
        let mut first: Option<usize> = None;
        let mut wanted: Option<usize> = None;
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
                dflt.get_or_insert(lang_off);
            }
            first.get_or_insert(lang_off);
            if let ScriptSelector::Tag(want) = selector {
                if tag == want {
                    wanted.get_or_insert(lang_off);
                }
            }
        }
        match selector {
            ScriptSelector::Default => dflt.or(first),
            // A requested script: prefer it, then DFLT, then the first script
            // (so a font that only tags `latn` still resolves for Latin text).
            ScriptSelector::Tag(_) => wanted.or(dflt).or(first),
        }
    }
}

/// Which script's language system to resolve features under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptSelector {
    /// DFLT script, else the first script (legacy Latin behaviour).
    Default,
    /// A specific OpenType script tag (`latn`, `arab`, `hebr`, …).
    Tag([u8; 4]),
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

/// Coverage index of `glyph`, or `None` when the glyph is not covered. Mirrors
/// [`parse_coverage`] ordering without materialising the whole list.
fn coverage_index(d: &[u8], off: usize, glyph: u16) -> Option<u16> {
    match be16(d, off) {
        1 => {
            let count = be16(d, off + 2) as usize;
            for i in 0..count {
                if be16(d, off + 4 + i * 2) == glyph {
                    return Some(i as u16);
                }
            }
            None
        }
        2 => {
            let range_count = be16(d, off + 2) as usize;
            for i in 0..range_count {
                let r = off + 4 + i * 6;
                let start = be16(d, r);
                let end = be16(d, r + 2);
                let start_cov = be16(d, r + 4);
                if glyph >= start && glyph <= end {
                    return Some(start_cov + (glyph - start));
                }
            }
            None
        }
        _ => None,
    }
}

/// Whether `glyph` is in the Coverage table at `off`.
fn coverage_contains(d: &[u8], off: usize, glyph: u16) -> bool {
    coverage_index(d, off, glyph).is_some()
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

/// Class of `glyph` in a ClassDef at `off` (0 when absent), read on demand.
fn class_of(d: &[u8], off: usize, glyph: u16) -> u16 {
    match be16(d, off) {
        1 => {
            let start = be16(d, off + 2);
            let count = be16(d, off + 4) as usize;
            if glyph >= start && ((glyph - start) as usize) < count {
                be16(d, off + 6 + (glyph - start) as usize * 2)
            } else {
                0
            }
        }
        2 => {
            let range_count = be16(d, off + 2) as usize;
            for i in 0..range_count {
                let r = off + 4 + i * 6;
                let start = be16(d, r);
                let end = be16(d, r + 2);
                if glyph >= start && glyph <= end {
                    return be16(d, r + 4);
                }
            }
            0
        }
        _ => 0,
    }
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

// ════════════════════════════════════════════════════════════════════════════
//  Complex shaping (positioned): script selection, GSUB contextual + joining,
//  GPOS kerning + mark positioning. Lives in `ComplexShaper`, built lazily from
//  the same `TrueTypeFont`. The Latin maps above are untouched.
// ════════════════════════════════════════════════════════════════════════════

/// A shaped glyph with its final placement, in font units. `x_offset`/`y_offset`
/// shift the glyph from the pen position without consuming advance (used for
/// mark attachment); `x_advance` is how far the pen moves after drawing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// Final glyph id (after substitution).
    pub gid: u16,
    /// Pen x-advance after this glyph, in font units.
    pub x_advance: i32,
    /// x placement offset from the pen, font units (marks attach via this).
    pub x_offset: i32,
    /// y placement offset from the baseline, font units (marks ride above/below).
    pub y_offset: i32,
    /// Index into the original character run this glyph derives from (cluster).
    pub cluster: usize,
}

/// GDEF glyph classes (subset we use). Mark = 3.
const GDEF_CLASS_MARK: u16 = 3;

/// An anchor point in font units.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    x: i16,
    y: i16,
}

/// The four Arabic joining types we assign (Transparent marks join through).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Join {
    /// Non-joining (isolated): U, space, non-cursive.
    Isolated,
    /// Right-joining (joins to a preceding letter only): e.g. Alef, Dal.
    Right,
    /// Dual-joining (joins on both sides): most Arabic letters.
    Dual,
    /// Join-causing (Tatweel) — behaves like dual for shaping.
    Causing,
    /// Transparent (marks): does not break the cursive connection.
    Transparent,
}

/// The font bytes plus the GSUB table base — the two constants threaded through
/// the whole (recursive) GSUB-application family. Bundling them keeps the
/// per-call argument lists short and makes the nested-lookup offset arithmetic
/// read against a single `base`.
#[derive(Clone, Copy)]
struct Gsub<'a> {
    d: &'a [u8],
    base: usize,
}

/// The three ClassDef offsets a format-2 chaining subtable applies to its
/// backtrack / input / lookahead glyph sequences (kept together so the matcher
/// takes one reference instead of three scalars).
#[derive(Clone, Copy)]
struct ChainClassDefs {
    back: usize,
    input: usize,
    look: usize,
}

/// Lazily-built positioned shaper carrying the raw `GSUB`/`GPOS`/`GDEF` ranges,
/// resolved against a chosen script when [`ComplexShaper::shape`] runs.
#[derive(Debug, Clone)]
pub struct ComplexShaper {
    data: Vec<u8>,
    gsub: Option<usize>,
    gpos: Option<usize>,
    gdef: Option<usize>,
}

impl ComplexShaper {
    /// Build from a font, copying only the table bytes we need so the shaper is
    /// self-contained. Returns `None` when the font has no layout tables.
    pub fn new(ttf: &TrueTypeFont) -> Option<ComplexShaper> {
        let gsub = ttf.gsub_range();
        let gpos = ttf.gpos_range();
        let gdef = ttf.gdef_range();
        if gsub.is_none() && gpos.is_none() {
            return None;
        }
        let data = ttf.data();
        let valid = |r: Option<(usize, usize)>| {
            r.and_then(|(o, l)| if o + l <= data.len() { Some(o) } else { None })
        };
        Some(ComplexShaper {
            data: data.to_vec(),
            gsub: valid(gsub),
            gpos: valid(gpos),
            gdef: valid(gdef),
        })
    }

    /// Shape a run of already-mapped glyphs into positioned glyphs for the given
    /// script tag (`latn`, `arab`, `hebr`, …). `base_advance` yields each glyph's
    /// `hmtx` advance in font units. Substitutions run first (script features +,
    /// for cursive scripts, Arabic joining), then GPOS kerning and mark
    /// positioning. The caller maps Unicode → glyph and provides per-glyph
    /// cursive joining decisions are derived here from `unicodes`.
    pub fn shape(
        &self,
        gids: &[u16],
        unicodes: &[u32],
        script: [u8; 4],
        base_advance: &dyn Fn(u16) -> i32,
    ) -> Vec<ShapedGlyph> {
        let d = &self.data;
        let mut buf: Vec<ShapedGlyph> = gids
            .iter()
            .enumerate()
            .map(|(i, &gid)| ShapedGlyph {
                gid,
                x_advance: 0,
                x_offset: 0,
                y_offset: 0,
                cluster: i,
            })
            .collect();

        // ── GSUB ─────────────────────────────────────────────────────────────
        if let Some(gsub_base) = self.gsub {
            let gsub = Gsub {
                d,
                base: gsub_base,
            };
            // Arabic joining features (init/medi/fina/isol) for cursive scripts:
            // decide the form of each glyph from the Unicode joining classes,
            // then apply only the matching single-substitution feature per glyph.
            if is_cursive_script(script) {
                self.apply_arabic_joining(&gsub, script, unicodes, &mut buf);
            }
            // Standard substitution features in a sensible order. ccmp first
            // (compose marks), then ligatures, then contextual.
            let features: [[u8; 4]; 6] =
                [*b"ccmp", *b"rlig", *b"liga", *b"clig", *b"calt", *b"locl"];
            let lookups = self.feature_lookups(d, gsub_base, script, &features);
            for lk in lookups {
                self.apply_gsub_lookup(&gsub, lk, 0, &mut buf);
            }
        }

        // Seed advances from hmtx now that substitution settled the glyph ids.
        for g in &mut buf {
            g.x_advance = base_advance(g.gid);
        }

        // ── GPOS ─────────────────────────────────────────────────────────────
        if let Some(gpos) = self.gpos {
            let features: [[u8; 4]; 3] = [*b"kern", *b"mark", *b"mkmk"];
            let lookups = self.feature_lookups(d, gpos, script, &features);
            for lk in lookups {
                self.apply_gpos_lookup(d, lk, &mut buf);
            }
        }

        buf
    }

    /// Resolve feature lookups for a script in LookupList order (positioned path
    /// applies lookups in table order, which is what OpenType specifies).
    fn feature_lookups(
        &self,
        d: &[u8],
        base: usize,
        script: [u8; 4],
        wanted: &[[u8; 4]],
    ) -> Vec<usize> {
        let lookup_list = base + be16(d, base + 8) as usize;
        if be16(d, base + 8) == 0 {
            return Vec::new();
        }
        let script_list = base + be16(d, base + 4) as usize;
        let feature_list = base + be16(d, base + 6) as usize;
        let lang_sys = match self.select_lang_sys(d, script_list, script) {
            Some(o) => o,
            None => return Vec::new(),
        };
        let feature_index_count = be16(d, lang_sys + 4) as usize;
        let feature_count = be16(d, feature_list) as usize;
        let mut indices: Vec<u16> = Vec::new();
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
                indices.push(be16(d, feature_off + 4 + j * 2));
            }
        }
        indices.sort_unstable();
        indices.dedup();
        let lookup_count = be16(d, lookup_list) as usize;
        indices
            .into_iter()
            .filter_map(|li| {
                let li = li as usize;
                (li < lookup_count).then(|| lookup_list + be16(d, lookup_list + 2 + li * 2) as usize)
            })
            .collect()
    }

    /// Resolve just the lookups for a single feature tag under a script (used by
    /// Arabic joining where each form maps to one feature).
    fn single_feature_lookups(
        &self,
        d: &[u8],
        base: usize,
        script: [u8; 4],
        tag: [u8; 4],
    ) -> Vec<usize> {
        self.feature_lookups(d, base, script, &[tag])
    }

    /// LanguageSystem offset for a specific script, falling back to DFLT/first.
    fn select_lang_sys(&self, d: &[u8], script_list: usize, script: [u8; 4]) -> Option<usize> {
        let probe = Shaper::default();
        probe.select_lang_sys(d, script_list, ScriptSelector::Tag(script))
    }

    // ── Arabic joining ────────────────────────────────────────────────────────

    /// Assign init/medi/fina/isol forms from the Unicode joining classes and
    /// apply the corresponding GSUB single substitution to each glyph.
    fn apply_arabic_joining(
        &self,
        gsub: &Gsub,
        script: [u8; 4],
        unicodes: &[u32],
        buf: &mut [ShapedGlyph],
    ) {
        let d = gsub.d;
        let n = buf.len();
        if n == 0 {
            return;
        }
        // Join type per position (Transparent marks are skipped for context).
        let join: Vec<Join> = unicodes.iter().map(|&u| joining_type(u)).collect();

        // For each non-transparent letter, look at the previous/next
        // non-transparent letter to decide whether it connects on each side.
        let prev_letter = |i: usize| -> Option<usize> {
            let mut k = i;
            while k > 0 {
                k -= 1;
                if join.get(k).copied().unwrap_or(Join::Isolated) != Join::Transparent {
                    return Some(k);
                }
            }
            None
        };
        let next_letter = |i: usize| -> Option<usize> {
            let mut k = i + 1;
            while k < n {
                if join.get(k).copied().unwrap_or(Join::Isolated) != Join::Transparent {
                    return Some(k);
                }
                k += 1;
            }
            None
        };

        // Pre-resolve the four feature lookup sets once.
        let isol = self.single_feature_lookups(d, gsub.base, script, *b"isol");
        let init = self.single_feature_lookups(d, gsub.base, script, *b"init");
        let medi = self.single_feature_lookups(d, gsub.base, script, *b"medi");
        let fina = self.single_feature_lookups(d, gsub.base, script, *b"fina");

        // Decide each glyph's form (and thus which feature's lookups apply) in a
        // first pass driven by the joining classes, then apply — separating the
        // positional context from the glyph mutation. Transparent/non-joining
        // positions select the isolated set (a no-op on an already-isolated
        // glyph), so the apply loop needs no per-glyph branch.
        let forms: Vec<&Vec<usize>> = (0..n)
            .map(|i| {
                let jt = join.get(i).copied().unwrap_or(Join::Isolated);
                if jt == Join::Transparent || jt == Join::Isolated {
                    return &isol;
                }
                // The previous letter connects to our left only if it is dual or
                // join-causing (right-joining letters do not connect rightward).
                let joins_prev = prev_letter(i).is_some_and(|p| {
                    matches!(
                        join.get(p).copied().unwrap_or(Join::Isolated),
                        Join::Dual | Join::Causing
                    )
                });
                // We connect to the next letter only if we are dual/causing and
                // the next letter accepts a connection on its left side.
                let joins_next = (jt == Join::Dual || jt == Join::Causing)
                    && next_letter(i).is_some_and(|q| {
                        matches!(
                            join.get(q).copied().unwrap_or(Join::Isolated),
                            Join::Dual | Join::Right | Join::Causing
                        )
                    });
                // joins both sides → medial; next only → initial; prev only →
                // final; neither → isolated.
                match (joins_prev, joins_next) {
                    (true, true) => &medi,
                    (false, true) => &init,
                    (true, false) => &fina,
                    (false, false) => &isol,
                }
            })
            .collect();

        for (g, lookups) in buf.iter_mut().zip(forms.iter()) {
            let mut one = [*g];
            for &lk in lookups.iter() {
                self.apply_gsub_lookup(gsub, lk, 0, &mut one);
            }
            *g = one[0];
        }
    }

    // ── GSUB application (positioned buffer) ──────────────────────────────────
    //
    // The GSUB-application family threads two constants — the font bytes `d` and
    // the GSUB table base — through every (possibly nested) call. They are
    // bundled into a [`Gsub`] context so each method carries one borrow instead
    // of two scalars, and a recursion `depth` guards nested SubstLookupRecords.

    fn apply_gsub_lookup(&self, g: &Gsub, lookup_off: usize, depth: u8, buf: &mut [ShapedGlyph]) {
        if depth > 8 {
            return;
        }
        let d = g.d;
        let lookup_type = be16(d, lookup_off);
        let subtable_count = be16(d, lookup_off + 4) as usize;
        for i in 0..subtable_count {
            let sub_off = lookup_off + be16(d, lookup_off + 6 + i * 2) as usize;
            self.apply_gsub_subtable(g, lookup_type, sub_off, depth, buf);
        }
    }

    fn apply_gsub_subtable(
        &self,
        g: &Gsub,
        lookup_type: u16,
        sub_off: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) {
        match lookup_type {
            1 => self.apply_single_subst(g.d, sub_off, buf),
            5 => self.apply_context_subst(g, sub_off, depth, buf),
            6 => self.apply_chain_context_subst(g, sub_off, depth, buf),
            7 => {
                if be16(g.d, sub_off) == 1 {
                    let real_type = be16(g.d, sub_off + 2);
                    let real_off = sub_off + be32(g.d, sub_off + 4) as usize;
                    if real_type != 7 {
                        self.apply_gsub_subtable(g, real_type, real_off, depth, buf);
                    }
                }
            }
            // Types 4 (ligature) and 2/3 reorder/merge glyphs, which the slice
            // buffer cannot express; the Latin path handles ligatures.
            _ => {}
        }
    }

    /// Apply a GSUB single substitution (types 1.1/1.2) to every covered glyph.
    fn apply_single_subst(&self, d: &[u8], off: usize, buf: &mut [ShapedGlyph]) {
        let format = be16(d, off);
        let coverage_off = off + be16(d, off + 2) as usize;
        match format {
            1 => {
                let delta = bei16(d, off + 4);
                for g in buf.iter_mut() {
                    if coverage_contains(d, coverage_off, g.gid) {
                        g.gid = (g.gid as i32 + delta as i32) as u16;
                    }
                }
            }
            2 => {
                let count = be16(d, off + 4) as usize;
                for g in buf.iter_mut() {
                    if let Some(ci) = coverage_index(d, coverage_off, g.gid) {
                        let ci = ci as usize;
                        if ci < count {
                            g.gid = be16(d, off + 6 + ci * 2);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Apply a contextual substitution (GSUB type 5). Only formats 1 & 2 are
    /// handled (sequence rules by glyph and by class); format 3 is folded into
    /// the chaining handler. Nested lookups recurse with the same buffer.
    fn apply_context_subst(&self, g: &Gsub, off: usize, depth: u8, buf: &mut [ShapedGlyph]) {
        let d = g.d;
        let format = be16(d, off);
        match format {
            1 => {
                let coverage_off = off + be16(d, off + 2) as usize;
                let rule_set_count = be16(d, off + 4) as usize;
                let mut i = 0;
                while i < buf.len() {
                    if let Some(ci) = coverage_index(d, coverage_off, buf[i].gid) {
                        let ci = ci as usize;
                        if ci < rule_set_count {
                            let set_off = off + be16(d, off + 6 + ci * 2) as usize;
                            if self.try_context_format1(g, set_off, i, depth, buf) {
                                i += 1;
                                continue;
                            }
                        }
                    }
                    i += 1;
                }
            }
            2 => {
                let coverage_off = off + be16(d, off + 2) as usize;
                let class_def = off + be16(d, off + 4) as usize;
                let rule_set_count = be16(d, off + 6) as usize;
                let mut i = 0;
                while i < buf.len() {
                    if coverage_contains(d, coverage_off, buf[i].gid) {
                        let cls = class_of(d, class_def, buf[i].gid) as usize;
                        if cls < rule_set_count {
                            let set_ptr = be16(d, off + 8 + cls * 2);
                            if set_ptr != 0 {
                                let set_off = off + set_ptr as usize;
                                if self.try_context_format2(g, set_off, class_def, i, depth, buf) {
                                    i += 1;
                                    continue;
                                }
                            }
                        }
                    }
                    i += 1;
                }
            }
            3 => self.apply_chain_context_subst(g, off, depth, buf),
            _ => {}
        }
    }

    /// Format-1 context: glyph-id sequence rules in a RuleSet.
    fn try_context_format1(
        &self,
        g: &Gsub,
        set_off: usize,
        pos: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) -> bool {
        let d = g.d;
        let rule_count = be16(d, set_off) as usize;
        for r in 0..rule_count {
            let rule_off = set_off + be16(d, set_off + 2 + r * 2) as usize;
            let glyph_count = be16(d, rule_off) as usize;
            let subst_count = be16(d, rule_off + 2) as usize;
            if glyph_count == 0 {
                continue;
            }
            // input[0] is the covered glyph (implicit); input[1..] follow.
            let mut ok = true;
            for k in 1..glyph_count {
                let want = be16(d, rule_off + 4 + (k - 1) * 2);
                match buf.get(pos + k) {
                    Some(g) if g.gid == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let subst_base = rule_off + 4 + (glyph_count - 1) * 2;
            self.apply_subst_records(g, subst_base, subst_count, pos, depth, buf);
            return true;
        }
        false
    }

    /// Format-2 context: class sequence rules in a RuleSet.
    fn try_context_format2(
        &self,
        g: &Gsub,
        set_off: usize,
        class_def: usize,
        pos: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) -> bool {
        let d = g.d;
        let rule_count = be16(d, set_off) as usize;
        for r in 0..rule_count {
            let rule_off = set_off + be16(d, set_off + 2 + r * 2) as usize;
            let glyph_count = be16(d, rule_off) as usize;
            let subst_count = be16(d, rule_off + 2) as usize;
            if glyph_count == 0 {
                continue;
            }
            let mut ok = true;
            for k in 1..glyph_count {
                let want = be16(d, rule_off + 4 + (k - 1) * 2);
                match buf.get(pos + k) {
                    Some(g) if class_of(d, class_def, g.gid) == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let subst_base = rule_off + 4 + (glyph_count - 1) * 2;
            self.apply_subst_records(g, subst_base, subst_count, pos, depth, buf);
            return true;
        }
        false
    }

    /// Apply a chaining contextual substitution (GSUB type 6). Formats 1/2/3 are
    /// handled; the common format-3 (coverage backtrack/input/lookahead) is the
    /// one fonts actually ship for `calt`/`clig`.
    fn apply_chain_context_subst(&self, g: &Gsub, off: usize, depth: u8, buf: &mut [ShapedGlyph]) {
        let d = g.d;
        match be16(d, off) {
            1 => {
                let coverage_off = off + be16(d, off + 2) as usize;
                let rule_set_count = be16(d, off + 4) as usize;
                let mut i = 0;
                while i < buf.len() {
                    if let Some(ci) = coverage_index(d, coverage_off, buf[i].gid) {
                        let ci = ci as usize;
                        if ci < rule_set_count {
                            let set_off = off + be16(d, off + 6 + ci * 2) as usize;
                            self.try_chain_format1(g, set_off, i, depth, buf);
                        }
                    }
                    i += 1;
                }
            }
            2 => {
                let coverage_off = off + be16(d, off + 2) as usize;
                let cds = ChainClassDefs {
                    back: off + be16(d, off + 4) as usize,
                    input: off + be16(d, off + 6) as usize,
                    look: off + be16(d, off + 8) as usize,
                };
                let rule_set_count = be16(d, off + 10) as usize;
                let mut i = 0;
                while i < buf.len() {
                    if coverage_contains(d, coverage_off, buf[i].gid) {
                        let cls = class_of(d, cds.input, buf[i].gid) as usize;
                        if cls < rule_set_count {
                            let set_ptr = be16(d, off + 12 + cls * 2);
                            if set_ptr != 0 {
                                let set_off = off + set_ptr as usize;
                                self.try_chain_format2(g, set_off, &cds, i, depth, buf);
                            }
                        }
                    }
                    i += 1;
                }
            }
            3 => {
                let mut i = 0;
                while i < buf.len() {
                    self.try_chain_format3(g, off, i, depth, buf);
                    i += 1;
                }
            }
            _ => {}
        }
    }

    /// Format-1 chaining: glyph-id backtrack/input/lookahead in a ChainRuleSet.
    fn try_chain_format1(
        &self,
        g: &Gsub,
        set_off: usize,
        pos: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) -> bool {
        let d = g.d;
        let rule_count = be16(d, set_off) as usize;
        for r in 0..rule_count {
            let rule_off = set_off + be16(d, set_off + 2 + r * 2) as usize;
            let mut p = rule_off;
            let back_count = be16(d, p) as usize;
            p += 2;
            // Backtrack is stored in reverse text order.
            let mut ok = true;
            for k in 0..back_count {
                let want = be16(d, p + k * 2);
                match pos.checked_sub(k + 1).and_then(|j| buf.get(j)) {
                    Some(g) if g.gid == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            p += back_count * 2;
            if !ok {
                continue;
            }
            let input_count = be16(d, p) as usize;
            p += 2;
            for k in 1..input_count {
                let want = be16(d, p + (k - 1) * 2);
                match buf.get(pos + k) {
                    Some(g) if g.gid == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            p += input_count.saturating_sub(1) * 2;
            if !ok {
                continue;
            }
            let look_count = be16(d, p) as usize;
            p += 2;
            for k in 0..look_count {
                let want = be16(d, p + k * 2);
                match buf.get(pos + input_count + k) {
                    Some(g) if g.gid == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            p += look_count * 2;
            if !ok {
                continue;
            }
            let subst_count = be16(d, p) as usize;
            self.apply_subst_records(g, p + 2, subst_count, pos, depth, buf);
            return true;
        }
        false
    }

    /// Format-2 chaining: class-based backtrack/input/lookahead. The three
    /// ClassDef offsets travel together in [`ChainClassDefs`].
    fn try_chain_format2(
        &self,
        g: &Gsub,
        set_off: usize,
        cds: &ChainClassDefs,
        pos: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) -> bool {
        let d = g.d;
        let (back_cd, input_cd, look_cd) = (cds.back, cds.input, cds.look);
        let rule_count = be16(d, set_off) as usize;
        for r in 0..rule_count {
            let rule_off = set_off + be16(d, set_off + 2 + r * 2) as usize;
            let mut p = rule_off;
            let back_count = be16(d, p) as usize;
            p += 2;
            let mut ok = true;
            for k in 0..back_count {
                let want = be16(d, p + k * 2);
                match pos.checked_sub(k + 1).and_then(|j| buf.get(j)) {
                    Some(g) if class_of(d, back_cd, g.gid) == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            p += back_count * 2;
            if !ok {
                continue;
            }
            let input_count = be16(d, p) as usize;
            p += 2;
            for k in 1..input_count {
                let want = be16(d, p + (k - 1) * 2);
                match buf.get(pos + k) {
                    Some(g) if class_of(d, input_cd, g.gid) == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            p += input_count.saturating_sub(1) * 2;
            if !ok {
                continue;
            }
            let look_count = be16(d, p) as usize;
            p += 2;
            for k in 0..look_count {
                let want = be16(d, p + k * 2);
                match buf.get(pos + input_count + k) {
                    Some(g) if class_of(d, look_cd, g.gid) == want => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            p += look_count * 2;
            if !ok {
                continue;
            }
            let subst_count = be16(d, p) as usize;
            self.apply_subst_records(g, p + 2, subst_count, pos, depth, buf);
            return true;
        }
        false
    }

    /// Format-3 chaining: coverage arrays for backtrack/input/lookahead.
    fn try_chain_format3(
        &self,
        g: &Gsub,
        off: usize,
        pos: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) -> bool {
        let d = g.d;
        let mut p = off + 2;
        let back_count = be16(d, p) as usize;
        p += 2;
        // Backtrack coverages are in reverse text order.
        for k in 0..back_count {
            let cov = off + be16(d, p + k * 2) as usize;
            match pos.checked_sub(k + 1).and_then(|j| buf.get(j)) {
                Some(g) if coverage_contains(d, cov, g.gid) => {}
                _ => return false,
            }
        }
        p += back_count * 2;
        let input_count = be16(d, p) as usize;
        p += 2;
        if input_count == 0 {
            return false;
        }
        for k in 0..input_count {
            let cov = off + be16(d, p + k * 2) as usize;
            match buf.get(pos + k) {
                Some(g) if coverage_contains(d, cov, g.gid) => {}
                _ => return false,
            }
        }
        p += input_count * 2;
        let look_count = be16(d, p) as usize;
        p += 2;
        for k in 0..look_count {
            let cov = off + be16(d, p + k * 2) as usize;
            match buf.get(pos + input_count + k) {
                Some(g) if coverage_contains(d, cov, g.gid) => {}
                _ => return false,
            }
        }
        p += look_count * 2;
        let subst_count = be16(d, p) as usize;
        self.apply_subst_records(g, p + 2, subst_count, pos, depth, buf);
        true
    }

    /// Apply a run of SubstLookupRecords (sequenceIndex, lookupListIndex) at the
    /// matched position: each names a nested GSUB lookup to run at `pos + idx`.
    fn apply_subst_records(
        &self,
        g: &Gsub,
        records_off: usize,
        count: usize,
        pos: usize,
        depth: u8,
        buf: &mut [ShapedGlyph],
    ) {
        let d = g.d;
        let lookup_list = g.base + be16(d, g.base + 8) as usize;
        let lookup_count = be16(d, lookup_list) as usize;
        for i in 0..count {
            let rec = records_off + i * 4;
            let seq_index = be16(d, rec) as usize;
            let lookup_index = be16(d, rec + 2) as usize;
            if lookup_index >= lookup_count {
                continue;
            }
            let nested = lookup_list + be16(d, lookup_list + 2 + lookup_index * 2) as usize;
            let at = pos + seq_index;
            if at < buf.len() {
                // Nested lookups operate on the glyph at `at` (and may look
                // around it). Pass the tail starting at `at` so positional
                // lookups see following context, but mutate in place.
                self.apply_gsub_lookup(g, nested, depth + 1, &mut buf[at..]);
            }
        }
    }

    // ── GPOS application (positioned buffer) ──────────────────────────────────

    fn apply_gpos_lookup(&self, d: &[u8], lookup_off: usize, buf: &mut [ShapedGlyph]) {
        let lookup_type = be16(d, lookup_off);
        let subtable_count = be16(d, lookup_off + 4) as usize;
        for i in 0..subtable_count {
            let sub_off = lookup_off + be16(d, lookup_off + 6 + i * 2) as usize;
            self.apply_gpos_subtable(d, lookup_type, sub_off, buf);
        }
    }

    /// Apply a single GPOS subtable. Type 9 (extension) unwraps to the real
    /// subtable in one hop; GPOS has no nested-lookup mechanism, so this never
    /// needs the table base or a recursion depth.
    fn apply_gpos_subtable(
        &self,
        d: &[u8],
        lookup_type: u16,
        sub_off: usize,
        buf: &mut [ShapedGlyph],
    ) {
        match lookup_type {
            2 => self.apply_pair_pos(d, sub_off, buf),
            4 => self.apply_mark_to_base(d, sub_off, buf),
            5 => self.apply_mark_to_ligature(d, sub_off, buf),
            6 => self.apply_mark_to_mark(d, sub_off, buf),
            9 => {
                if be16(d, sub_off) == 1 {
                    let real_type = be16(d, sub_off + 2);
                    let real_off = sub_off + be32(d, sub_off + 4) as usize;
                    // Guard against a type-9 pointing at another type-9.
                    if real_type != 9 {
                        self.apply_gpos_subtable(d, real_type, real_off, buf);
                    }
                }
            }
            _ => {}
        }
    }

    /// GPOS pair adjustment (type 2) on the positioned buffer: fold the first
    /// glyph's XAdvance adjustment into its advance.
    fn apply_pair_pos(&self, d: &[u8], off: usize, buf: &mut [ShapedGlyph]) {
        let format = be16(d, off);
        let coverage_off = off + be16(d, off + 2) as usize;
        let vf1 = be16(d, off + 4);
        let vf2 = be16(d, off + 6);
        let v1_size = value_record_size(vf1);
        let v2_size = value_record_size(vf2);
        if !value_has_xadvance(vf1) {
            return;
        }
        match format {
            1 => {
                let pair_set_count = be16(d, off + 8) as usize;
                let record_size = 2 + v1_size + v2_size;
                let mut i = 0;
                while i + 1 < buf.len() {
                    if let Some(ci) = coverage_index(d, coverage_off, buf[i].gid) {
                        let ci = ci as usize;
                        if ci < pair_set_count {
                            let set_off = off + be16(d, off + 10 + ci * 2) as usize;
                            let pair_count = be16(d, set_off) as usize;
                            for j in 0..pair_count {
                                let rec = set_off + 2 + j * record_size;
                                if be16(d, rec) == buf[i + 1].gid {
                                    buf[i].x_advance += bei16(d, rec + 2) as i32;
                                    break;
                                }
                            }
                        }
                    }
                    i += 1;
                }
            }
            2 => {
                let class_def1 = off + be16(d, off + 8) as usize;
                let class_def2 = off + be16(d, off + 10) as usize;
                let class1_count = be16(d, off + 12) as usize;
                let class2_count = be16(d, off + 14) as usize;
                let record_size = v1_size + v2_size;
                if class1_count == 0 || class2_count == 0 {
                    return;
                }
                let base_rec = off + 16;
                let mut i = 0;
                while i + 1 < buf.len() {
                    if coverage_contains(d, coverage_off, buf[i].gid) {
                        let c1 = class_of(d, class_def1, buf[i].gid) as usize;
                        let c2 = class_of(d, class_def2, buf[i + 1].gid) as usize;
                        if c1 < class1_count && c2 < class2_count {
                            let rec = base_rec + (c1 * class2_count + c2) * record_size;
                            buf[i].x_advance += bei16(d, rec) as i32;
                        }
                    }
                    i += 1;
                }
            }
            _ => {}
        }
    }

    /// GPOS mark-to-base (type 4): attach each mark glyph's anchor to the
    /// matching anchor on the preceding base glyph.
    fn apply_mark_to_base(&self, d: &[u8], off: usize, buf: &mut [ShapedGlyph]) {
        if be16(d, off) != 1 {
            return;
        }
        let mark_cov = off + be16(d, off + 2) as usize;
        let base_cov = off + be16(d, off + 4) as usize;
        let mark_class_count = be16(d, off + 6) as usize;
        let mark_array = off + be16(d, off + 8) as usize;
        let base_array = off + be16(d, off + 10) as usize;
        if mark_class_count == 0 {
            return;
        }
        for i in 0..buf.len() {
            let mark_idx = match coverage_index(d, mark_cov, buf[i].gid) {
                Some(ci) => ci as usize,
                None => continue,
            };
            // Nearest preceding base glyph that is in the base coverage.
            let base_pos = match self.preceding_base(d, base_cov, buf, i) {
                Some(p) => p,
                None => continue,
            };
            let base_idx = match coverage_index(d, base_cov, buf[base_pos].gid) {
                Some(ci) => ci as usize,
                None => continue,
            };
            if let Some((mark_class, mark_anchor)) = mark_record(d, mark_array, mark_idx) {
                if mark_class >= mark_class_count {
                    continue;
                }
                let base_anchor =
                    base_anchor_record(d, base_array, base_idx, mark_class, mark_class_count);
                if let Some(base_anchor) = base_anchor {
                    self.attach_mark(buf, i, base_pos, mark_anchor, base_anchor);
                }
            }
        }
    }

    /// GPOS mark-to-mark (type 6): attach a mark to the preceding mark (stacking
    /// diacritics). Same layout as mark-to-base but the "base" is a Mark2 array.
    fn apply_mark_to_mark(&self, d: &[u8], off: usize, buf: &mut [ShapedGlyph]) {
        if be16(d, off) != 1 {
            return;
        }
        let mark1_cov = off + be16(d, off + 2) as usize;
        let mark2_cov = off + be16(d, off + 4) as usize;
        let mark_class_count = be16(d, off + 6) as usize;
        let mark1_array = off + be16(d, off + 8) as usize;
        let mark2_array = off + be16(d, off + 10) as usize;
        if mark_class_count == 0 {
            return;
        }
        for i in 0..buf.len() {
            let m1_idx = match coverage_index(d, mark1_cov, buf[i].gid) {
                Some(ci) => ci as usize,
                None => continue,
            };
            // mark2 is the immediately preceding glyph that is a covered mark.
            let m2_pos = match self.preceding_in_coverage(d, mark2_cov, buf, i) {
                Some(p) => p,
                None => continue,
            };
            let m2_idx = match coverage_index(d, mark2_cov, buf[m2_pos].gid) {
                Some(ci) => ci as usize,
                None => continue,
            };
            if let Some((mark_class, mark_anchor)) = mark_record(d, mark1_array, m1_idx) {
                if mark_class >= mark_class_count {
                    continue;
                }
                if let Some(base_anchor) =
                    base_anchor_record(d, mark2_array, m2_idx, mark_class, mark_class_count)
                {
                    self.attach_mark(buf, i, m2_pos, mark_anchor, base_anchor);
                }
            }
        }
    }

    /// GPOS mark-to-ligature (type 5): attach a mark to the right component of a
    /// preceding ligature glyph. We approximate the component as the last one
    /// (most marks attach to the final component); if a per-cluster component is
    /// known it would refine this, but the slice buffer has no ligature spans.
    fn apply_mark_to_ligature(&self, d: &[u8], off: usize, buf: &mut [ShapedGlyph]) {
        if be16(d, off) != 1 {
            return;
        }
        let mark_cov = off + be16(d, off + 2) as usize;
        let lig_cov = off + be16(d, off + 4) as usize;
        let mark_class_count = be16(d, off + 6) as usize;
        let mark_array = off + be16(d, off + 8) as usize;
        let lig_array = off + be16(d, off + 10) as usize;
        if mark_class_count == 0 {
            return;
        }
        for i in 0..buf.len() {
            let mark_idx = match coverage_index(d, mark_cov, buf[i].gid) {
                Some(ci) => ci as usize,
                None => continue,
            };
            let lig_pos = match self.preceding_in_coverage(d, lig_cov, buf, i) {
                Some(p) => p,
                None => continue,
            };
            let lig_idx = match coverage_index(d, lig_cov, buf[lig_pos].gid) {
                Some(ci) => ci as usize,
                None => continue,
            };
            if let Some((mark_class, mark_anchor)) = mark_record(d, mark_array, mark_idx) {
                if mark_class >= mark_class_count {
                    continue;
                }
                if let Some(base_anchor) = ligature_anchor_record(
                    d,
                    lig_array,
                    lig_idx,
                    mark_class,
                    mark_class_count,
                ) {
                    self.attach_mark(buf, i, lig_pos, mark_anchor, base_anchor);
                }
            }
        }
    }

    /// Place mark glyph `mark` so its anchor coincides with the base anchor:
    /// the mark's offset becomes base_anchor − mark_anchor, relative to the base
    /// glyph's pen origin, then we back out the advances accumulated between the
    /// base and the mark so the mark lands on the base regardless of intervening
    /// zero-advance marks.
    fn attach_mark(
        &self,
        buf: &mut [ShapedGlyph],
        mark: usize,
        base: usize,
        mark_anchor: Anchor,
        base_anchor: Anchor,
    ) {
        // x of base anchor measured from the base glyph origin.
        let base_x = buf[base].x_offset + base_anchor.x as i32;
        let base_y = buf[base].y_offset + base_anchor.y as i32;
        // Sum advances of glyphs strictly between base and mark (they shift the
        // pen forward; the mark must compensate to sit over the base).
        let mut between = 0i32;
        for g in buf.iter().take(mark).skip(base) {
            between += g.x_advance;
        }
        buf[mark].x_offset = base_x - mark_anchor.x as i32 - between;
        buf[mark].y_offset = base_y - mark_anchor.y as i32;
        // Marks carry no advance of their own once attached.
        buf[mark].x_advance = 0;
    }

    /// Nearest glyph before `i` that is a base for mark attachment: in coverage
    /// and not itself a GDEF mark (so marks attach to letters, not to marks).
    fn preceding_base(
        &self,
        d: &[u8],
        base_cov: usize,
        buf: &[ShapedGlyph],
        i: usize,
    ) -> Option<usize> {
        let mut k = i;
        while k > 0 {
            k -= 1;
            if self.is_mark_glyph(buf[k].gid) {
                continue; // skip intervening marks
            }
            if coverage_contains(d, base_cov, buf[k].gid) {
                return Some(k);
            }
            return None; // first non-mark glyph isn't a covered base
        }
        None
    }

    /// Immediately preceding glyph that is in `cov` (used by mark-to-mark and
    /// mark-to-ligature, where the target is the directly preceding glyph).
    fn preceding_in_coverage(
        &self,
        d: &[u8],
        cov: usize,
        buf: &[ShapedGlyph],
        i: usize,
    ) -> Option<usize> {
        if i == 0 {
            return None;
        }
        let prev = i - 1;
        coverage_contains(d, cov, buf[prev].gid).then_some(prev)
    }

    /// Whether `gid` is a GDEF Mark-class glyph. When the font has no GDEF, fall
    /// back to "not a mark" (the coverage tables then gate attachment).
    fn is_mark_glyph(&self, gid: u16) -> bool {
        let gdef = match self.gdef {
            Some(g) => g,
            None => return false,
        };
        let d = &self.data;
        let class_off_rel = be16(d, gdef + 4);
        if class_off_rel == 0 {
            return false;
        }
        class_of(d, gdef + class_off_rel as usize, gid) == GDEF_CLASS_MARK
    }
}

/// Read a MarkRecord (markClass, markAnchorOffset) from a MarkArray at index
/// `idx`. Returns `(class, anchor)` when the anchor is present and resolvable.
fn mark_record(d: &[u8], mark_array: usize, idx: usize) -> Option<(usize, Anchor)> {
    let count = be16(d, mark_array) as usize;
    if idx >= count {
        return None;
    }
    let rec = mark_array + 2 + idx * 4;
    let class = be16(d, rec) as usize;
    let anchor_rel = be16(d, rec + 2);
    if anchor_rel == 0 {
        return None;
    }
    let anchor = parse_anchor(d, mark_array + anchor_rel as usize)?;
    Some((class, anchor))
}

/// Read the base anchor for `mark_class` from a BaseArray at base index
/// `base_idx`. The BaseArray rows are `mark_class_count` anchors wide.
fn base_anchor_record(
    d: &[u8],
    base_array: usize,
    base_idx: usize,
    mark_class: usize,
    mark_class_count: usize,
) -> Option<Anchor> {
    let count = be16(d, base_array) as usize;
    if base_idx >= count {
        return None;
    }
    let row = base_array + 2 + base_idx * mark_class_count * 2;
    let anchor_rel = be16(d, row + mark_class * 2);
    if anchor_rel == 0 {
        return None;
    }
    parse_anchor(d, base_array + anchor_rel as usize)
}

/// Read the anchor for `mark_class` from a LigatureArray. We use the **last**
/// component's anchor (the common attachment point for trailing marks).
fn ligature_anchor_record(
    d: &[u8],
    lig_array: usize,
    lig_idx: usize,
    mark_class: usize,
    mark_class_count: usize,
) -> Option<Anchor> {
    let lig_count = be16(d, lig_array) as usize;
    if lig_idx >= lig_count {
        return None;
    }
    let attach_off = lig_array + be16(d, lig_array + 2 + lig_idx * 2) as usize;
    let component_count = be16(d, attach_off) as usize;
    if component_count == 0 {
        return None;
    }
    // Last component's ComponentRecord.
    let comp = component_count - 1;
    let row = attach_off + 2 + comp * mark_class_count * 2;
    let anchor_rel = be16(d, row + mark_class * 2);
    if anchor_rel == 0 {
        return None;
    }
    parse_anchor(d, attach_off + anchor_rel as usize)
}

/// Parse an Anchor table (formats 1/2/3 share the x,y at +2,+4; the device/
/// contour refinements are ignored — the design coordinates suffice).
fn parse_anchor(d: &[u8], off: usize) -> Option<Anchor> {
    let format = be16(d, off);
    if !(1..=3).contains(&format) {
        return None;
    }
    Some(Anchor {
        x: bei16(d, off + 2),
        y: bei16(d, off + 4),
    })
}

/// Whether an OpenType script tag is a cursive (Arabic-style joining) script we
/// run the joining pass for.
fn is_cursive_script(script: [u8; 4]) -> bool {
    matches!(&script, b"arab" | b"syrc" | b"mong" | b"nko " | b"rohg" | b"adlm")
}

/// Arabic joining type of a Unicode scalar, from the Unicode joining classes
/// (the subset that matters for shaping). Marks (combining) are Transparent.
fn joining_type(u: u32) -> Join {
    // Combining marks (general categories Mn/Me, plus Arabic combining ranges)
    // are transparent to joining.
    if is_combining_mark(u) {
        return Join::Transparent;
    }
    match u {
        // Tatweel / Kashida — join-causing.
        0x0640 => Join::Causing,
        // Right-joining Arabic letters: Alef forms, Dal, Thal, Reh, Zain, Waw,
        // Alef Maksura, Teh Marbuta, and a few others.
        0x0622 | 0x0623 | 0x0624 | 0x0625 | 0x0627 | 0x0629 | 0x062F | 0x0630 | 0x0631
        | 0x0632 | 0x0648 | 0x0671..=0x0673 | 0x0675..=0x0677 | 0x0688..=0x0699 | 0x06C0
        | 0x06C3..=0x06CB | 0x06CD | 0x06CF | 0x06D2 | 0x06D3 | 0x06EE | 0x06EF => Join::Right,
        // Dual-joining: the bulk of Arabic letters (Beh..Yeh range), plus the
        // common extended/Persian/Urdu letters.
        0x0620 | 0x0626 | 0x0628 | 0x062A..=0x062E | 0x0633..=0x063F | 0x0641..=0x0647
        | 0x0649 | 0x064A | 0x066E | 0x066F | 0x0678..=0x0687 | 0x069A..=0x06BF | 0x06CC
        | 0x06CE | 0x06D0 | 0x06D1 | 0x06FA..=0x06FC | 0x06FF | 0x0750..=0x077F => Join::Dual,
        _ => Join::Isolated,
    }
}

/// Whether `u` is a combining mark (transparent to Arabic joining). Covers the
/// main combining-diacritic blocks plus Arabic combining marks.
fn is_combining_mark(u: u32) -> bool {
    matches!(u,
        0x0300..=0x036F   // Combining Diacritical Marks
        | 0x0483..=0x0489 // Cyrillic combining
        | 0x0591..=0x05BD | 0x05BF | 0x05C1 | 0x05C2 | 0x05C4 | 0x05C5 | 0x05C7 // Hebrew points
        | 0x0610..=0x061A // Arabic combining
        | 0x064B..=0x065F // Arabic harakat (fatha, kasra, …)
        | 0x0670          // Arabic superscript alef
        | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7 | 0x06E8 | 0x06EA..=0x06ED // Arabic marks
        | 0x0711          // Syriac letter superscript alaph
        | 0x0730..=0x074A // Syriac points
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20FF // Combining Diacritical Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
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
        font_with_tables(&[(tag, table)])
    }

    // Generalised: wrap an arbitrary set of extra tables (e.g. GSUB + GPOS +
    // GDEF together) into a minimal sfnt accepted by parse_metrics.
    fn font_with_tables(extra: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        use crate::font::cff_to_otf::assemble_sfnt;
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
        ];
        for (tag, t) in extra {
            tables.push((tag, t.clone()));
        }
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

    // ── complex-path test fixtures ───────────────────────────────────────────

    // A GSUB single substitution (type 1.2) bound to `feature` under `script`,
    // mapping each coverage glyph → the matching glyph in `sub`. Built so the
    // ComplexShaper script selection + single subst can be exercised.
    fn gsub_single_format2(
        script: &[u8; 4],
        feature: &[u8; 4],
        coverage: &[u16],
        sub: &[u16],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        let n = coverage.len() as u16;
        let script_list_off = 10u16;
        let script_table_off = script_list_off + 2 + 6;
        let langsys_off = script_table_off + 4;
        let feature_list_off = langsys_off + 8;
        let feature_off = feature_list_off + 8;
        let lookup_list_off = feature_off + 6;
        let lookup_off = lookup_list_off + 4;
        // SingleSubstFormat2: format(2)+coverageOff(2)+glyphCount(2)+glyphs(n*2)
        let single_off = lookup_off + 8;
        let coverage_off = single_off + 6 + n * 2;

        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b.extend_from_slice(&script_list_off.to_be_bytes());
        b.extend_from_slice(&feature_list_off.to_be_bytes());
        b.extend_from_slice(&lookup_list_off.to_be_bytes());
        // ScriptList
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(script);
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
        b.extend_from_slice(feature);
        b.extend_from_slice(&(feature_off - feature_list_off).to_be_bytes());
        // Feature
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // LookupList
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(lookup_off - lookup_list_off).to_be_bytes());
        // Lookup (type 1 single)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(single_off - lookup_off).to_be_bytes());
        // SingleSubstFormat2
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&(coverage_off - single_off).to_be_bytes());
        b.extend_from_slice(&n.to_be_bytes());
        for &g in sub {
            b.extend_from_slice(&g.to_be_bytes());
        }
        // Coverage format 1
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&n.to_be_bytes());
        for &g in coverage {
            b.extend_from_slice(&g.to_be_bytes());
        }
        b
    }

    #[test]
    fn complex_script_selection_picks_arabic_feature() {
        // An `arab`/`init` single subst mapping gid 5 → 105. Selecting script
        // `arab` must find and apply it; selecting `latn` must not.
        let gsub = gsub_single_format2(b"arab", b"init", &[5], &[105]);
        let font = font_with_layout(b"GSUB", gsub);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let cs = ComplexShaper::new(&ttf).expect("has layout");
        let adv = |_g: u16| 500;
        // arab + an initial-position dual letter → init applies → 105.
        // U+0628 (Beh) is dual-joining; a lone letter joins to nothing so it is
        // isolated — but we feed two so the first is initial.
        let out = cs.shape(&[5, 5], &[0x0628, 0x0628], *b"arab", &adv);
        assert_eq!(out[0].gid, 105, "initial form applied to first letter");
        // Selecting latn finds no arab langsys feature here → unchanged.
        let out_latn = cs.shape(&[5], &[0x0628], *b"latn", &adv);
        assert_eq!(out_latn[0].gid, 5, "latn does not trigger the arab feature");
    }

    // GSUB type 6 format 3 chaining: input coverage [trigger], one lookahead
    // coverage [next], substituting input via a nested type-1.2 lookup.
    // Layout: two lookups (0 = chain, 1 = single A→B); feature `calt` references
    // lookup 0; lookup 0's SubstLookupRecord points at lookup 1.
    fn gsub_chain_format3(
        trigger: u16,
        next: u16,
        from: u16,
        to: u16,
    ) -> Vec<u8> {
        let mut b = Vec::new();
        let script_list_off = 10u16;
        let script_table_off = script_list_off + 2 + 6;
        let langsys_off = script_table_off + 4;
        let feature_list_off = langsys_off + 8;
        let feature_off = feature_list_off + 8;
        // LookupList: count=2 + 2 offsets = 6 bytes.
        let lookup_list_off = feature_off + 6;
        let lookup0_off = lookup_list_off + 6; // chain lookup (type 6)
                                               // Lookup0: type(2)+flag(2)+subCnt(2)+subOff(2) = 8
        let chain_off = lookup0_off + 8;
        // ChainContextFormat3:
        //  format(2)=3
        //  backtrackCount(2)=0
        //  inputCount(2)=1, inputCoverage[1](2)
        //  lookaheadCount(2)=1, lookaheadCoverage[1](2)
        //  substCount(2)=1, substRecords[1] = (seqIndex(2)=0, lookupIndex(2)=1)
        //  = 2+2 + 2+2 + 2+2 + 2 + 4 = 18 bytes
        let cov_input_off = chain_off + 18;
        let cov_look_off = cov_input_off + 6; // each coverage fmt1 = 6 bytes
        let lookup1_off = cov_look_off + 6; // single subst lookup (type 1)
                                            // Lookup1: 8 bytes header
        let single_off = lookup1_off + 8;
        // SingleSubstFormat2: format(2)+covOff(2)+count(2)+glyph(2) = 8
        let single_cov_off = single_off + 8;

        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b.extend_from_slice(&script_list_off.to_be_bytes());
        b.extend_from_slice(&feature_list_off.to_be_bytes());
        b.extend_from_slice(&lookup_list_off.to_be_bytes());
        // ScriptList (latn)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(b"latn");
        b.extend_from_slice(&(script_table_off - script_list_off).to_be_bytes());
        b.extend_from_slice(&(langsys_off - script_table_off).to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0xFFFFu16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // FeatureList: calt → feature referencing lookup 0
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(b"calt");
        b.extend_from_slice(&(feature_off - feature_list_off).to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes()); // lookupListIndex 0
        // LookupList
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&(lookup0_off - lookup_list_off).to_be_bytes());
        b.extend_from_slice(&(lookup1_off - lookup_list_off).to_be_bytes());
        // Lookup0 (type 6 chaining)
        b.extend_from_slice(&6u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(chain_off - lookup0_off).to_be_bytes());
        // ChainContextFormat3
        b.extend_from_slice(&3u16.to_be_bytes()); // format
        b.extend_from_slice(&0u16.to_be_bytes()); // backtrackCount
        b.extend_from_slice(&1u16.to_be_bytes()); // inputCount
        b.extend_from_slice(&(cov_input_off - chain_off).to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes()); // lookaheadCount
        b.extend_from_slice(&(cov_look_off - chain_off).to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes()); // substCount
        b.extend_from_slice(&0u16.to_be_bytes()); // seqIndex
        b.extend_from_slice(&1u16.to_be_bytes()); // lookupListIndex 1
        // input coverage (trigger)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&trigger.to_be_bytes());
        // lookahead coverage (next)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&next.to_be_bytes());
        // Lookup1 (type 1 single)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(single_off - lookup1_off).to_be_bytes());
        // SingleSubstFormat2
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&(single_cov_off - single_off).to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&to.to_be_bytes());
        // coverage for single (from)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&from.to_be_bytes());
        b
    }

    #[test]
    fn complex_gsub_chaining_context_substitutes() {
        // calt: when glyph 30 is followed by glyph 31, substitute 30 → 130.
        let gsub = gsub_chain_format3(30, 31, 30, 130);
        let font = font_with_layout(b"GSUB", gsub);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let cs = ComplexShaper::new(&ttf).expect("has layout");
        let adv = |_g: u16| 500;
        // 30 followed by 31 → 30 becomes 130.
        let out = cs.shape(&[30, 31], &[b'a' as u32, b'b' as u32], *b"latn", &adv);
        assert_eq!(out[0].gid, 130, "chaining context fired");
        assert_eq!(out[1].gid, 31, "lookahead glyph untouched");
        // 30 NOT followed by 31 → no substitution.
        let out2 = cs.shape(&[30, 32], &[b'a' as u32, b'c' as u32], *b"latn", &adv);
        assert_eq!(out2[0].gid, 30, "no context → no substitution");
    }

    // GPOS mark-to-base (type 4): one base glyph (10) with an anchor at (300,700)
    // for mark class 0, and one mark glyph (20) whose anchor is (50,0). Bound to
    // the `mark` feature under `latn`. GDEF marks glyph 20 as a Mark.
    fn gpos_mark_to_base() -> Vec<u8> {
        let mut b = Vec::new();
        let script_list_off = 10u16;
        let script_table_off = script_list_off + 2 + 6;
        let langsys_off = script_table_off + 4;
        let feature_list_off = langsys_off + 8;
        let feature_off = feature_list_off + 8;
        let lookup_list_off = feature_off + 6;
        let lookup_off = lookup_list_off + 4;
        // MarkBasePosFormat1:
        //  format(2)=1, markCovOff(2), baseCovOff(2), markClassCount(2)=1,
        //  markArrayOff(2), baseArrayOff(2) = 12 bytes
        let markbase_off = lookup_off + 8;
        let mark_cov_off = markbase_off + 12;
        let base_cov_off = mark_cov_off + 6;
        let mark_array_off = base_cov_off + 6;
        // MarkArray: count(2)=1 + MarkRecord(class(2)+anchorOff(2)) = 6, then anchor.
        let mark_anchor_off = mark_array_off + 6;
        // Anchor fmt1: format(2)+x(2)+y(2) = 6.
        let base_array_off = mark_anchor_off + 6;
        // BaseArray: count(2)=1 + BaseRecord(markClassCount anchors → 1 off(2)) = 4, then anchor.
        let base_anchor_off = base_array_off + 4;

        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b.extend_from_slice(&script_list_off.to_be_bytes());
        b.extend_from_slice(&feature_list_off.to_be_bytes());
        b.extend_from_slice(&lookup_list_off.to_be_bytes());
        // ScriptList (latn)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(b"latn");
        b.extend_from_slice(&(script_table_off - script_list_off).to_be_bytes());
        b.extend_from_slice(&(langsys_off - script_table_off).to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0xFFFFu16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // FeatureList: mark
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(b"mark");
        b.extend_from_slice(&(feature_off - feature_list_off).to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // LookupList
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(lookup_off - lookup_list_off).to_be_bytes());
        // Lookup (type 4 mark-to-base)
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(markbase_off - lookup_off).to_be_bytes());
        // MarkBasePosFormat1
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&(mark_cov_off - markbase_off).to_be_bytes());
        b.extend_from_slice(&(base_cov_off - markbase_off).to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes()); // markClassCount
        b.extend_from_slice(&(mark_array_off - markbase_off).to_be_bytes());
        b.extend_from_slice(&(base_array_off - markbase_off).to_be_bytes());
        // mark coverage (glyph 20)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&20u16.to_be_bytes());
        // base coverage (glyph 10)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&10u16.to_be_bytes());
        // MarkArray
        b.extend_from_slice(&1u16.to_be_bytes()); // markCount
        b.extend_from_slice(&0u16.to_be_bytes()); // markClass
        b.extend_from_slice(&(mark_anchor_off - mark_array_off).to_be_bytes());
        // mark anchor (50, 0)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&50i16.to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
        // BaseArray
        b.extend_from_slice(&1u16.to_be_bytes()); // baseCount
        b.extend_from_slice(&(base_anchor_off - base_array_off).to_be_bytes());
        // base anchor (300, 700)
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&300i16.to_be_bytes());
        b.extend_from_slice(&700i16.to_be_bytes());
        b
    }

    // Minimal GDEF marking glyph 20 as Mark (class 3) via a ClassDef format 1.
    fn gdef_marking(gid: u16) -> Vec<u8> {
        let mut b = Vec::new();
        // GDEF header v1.0: version(4), glyphClassDefOff(2), attachListOff(2)=0,
        // ligCaretListOff(2)=0, markAttachClassDefOff(2)=0 = 12 bytes.
        let class_def_off = 12u16;
        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b.extend_from_slice(&class_def_off.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // ClassDef format 1: startGlyph(2)=gid, count(2)=1, classes[1](2)=3 (Mark)
        b.extend_from_slice(&1u16.to_be_bytes()); // classFormat
        b.extend_from_slice(&gid.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&GDEF_CLASS_MARK.to_be_bytes());
        b
    }

    #[test]
    fn complex_gpos_mark_to_base_attaches_diacritic() {
        // Base glyph 10, mark glyph 20. The mark must be offset so its anchor
        // (50,0) coincides with the base anchor (300,700).
        let gpos = gpos_mark_to_base();
        let gdef = gdef_marking(20);
        let font = font_with_tables(&[(b"GPOS", gpos), (b"GDEF", gdef)]);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let cs = ComplexShaper::new(&ttf).expect("has layout");
        let adv = |_g: u16| 500; // base advance 500
        let out = cs.shape(&[10, 20], &['a' as u32, 0x0301], *b"latn", &adv);
        assert_eq!(out.len(), 2);
        // Mark x_offset = base_x(300) - mark_x(50) - between_advances(base adv 500)
        //               = 300 - 50 - 500 = -250.
        assert_eq!(out[1].x_offset, -250, "mark x pulled back onto the base");
        // Mark y_offset = base_y(700) - mark_y(0) = 700.
        assert_eq!(out[1].y_offset, 700, "mark raised to the base anchor");
        // The attached mark carries no advance.
        assert_eq!(out[1].x_advance, 0, "attached mark has zero advance");
        // The base keeps its hmtx advance.
        assert_eq!(out[0].x_advance, 500);
    }

    #[test]
    fn complex_shaper_absent_without_layout() {
        // A font without GSUB/GPOS has no complex shaper.
        let font = font_with_layout(b"post", vec![0u8; 32]);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        assert!(ComplexShaper::new(&ttf).is_none());
    }

    #[test]
    fn arabic_joining_classes_are_assigned() {
        // Beh (dual), Alef (right), Tatweel (causing), space (non-joining),
        // fatha mark (transparent).
        assert_eq!(joining_type(0x0628), Join::Dual);
        assert_eq!(joining_type(0x0627), Join::Right);
        assert_eq!(joining_type(0x0640), Join::Causing);
        assert_eq!(joining_type(0x0020), Join::Isolated);
        assert_eq!(joining_type(0x064E), Join::Transparent);
    }

    #[test]
    fn malformed_layout_table_does_not_panic() {
        // Truncated/garbage GSUB bytes must degrade to a no-op, never panic.
        let mut junk = vec![0u8; 64];
        junk[0] = 0x00;
        junk[1] = 0x01; // pretend version 1.0
        junk[4] = 0xFF;
        junk[5] = 0xFF; // wild scriptList offset
        junk[8] = 0xFF;
        junk[9] = 0xFF; // wild lookupList offset
        let font = font_with_layout(b"GSUB", junk);
        let ttf = TrueTypeFont::parse_metrics(&font).expect("font parses");
        let shaper = Shaper::new(&ttf);
        // No rules extracted, no panic.
        let _ = shaper.substitute(&[1, 2, 3]);
        if let Some(cs) = ComplexShaper::new(&ttf) {
            let adv = |_g: u16| 500;
            let _ = cs.shape(&[1, 2, 3], &[1, 2, 3], *b"latn", &adv);
        }
    }
}
