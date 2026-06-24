//! Zero-dependency **Unicode Bidirectional Algorithm** (UAX #9) for line-level
//! reordering of mixed left-to-right / right-to-left text.
//!
//! The HTML engine already shapes each script run's glyphs (Arabic joining,
//! Hebrew letters) and places whitespace-split word boxes either left-to-right
//! or — for `direction: rtl` — right-to-left. What it lacked was the *bidi
//! reordering of runs within a line*: a line that mixes an RTL script with an
//! embedded Latin word or European/Arabic numbers (`"مرحبا ABC 123"`) must place
//! the Latin/number run upright at its own level, not naively reversed with the
//! rest of the line. This module computes the **resolved embedding level of
//! every character** of a line via the standard rules and exposes a [`reorder`]
//! that returns the visual order of the line's *word boxes* (the layout's
//! placeable atoms) per rule L2.
//!
//! Rules covered (the common, non-explicit cases — what real documents need):
//! - **P2–P3** — the paragraph (base) level is supplied by the caller from the
//!   element's CSS `direction` (`rtl` ⇒ 1, `ltr` ⇒ 0); we do not auto-detect it
//!   from the first strong character.
//! - **W1–W7** — weak types: NSM takes its predecessor's type (W1); EN→AN after
//!   AL (W2); AL→R (W3); a single ES/CS between two EN, or CS between two AN,
//!   joins them (W4); ET runs adjacent to EN become EN (W5); remaining
//!   separators/terminators become ON (W6); EN→L when the last strong was L
//!   (W7).
//! - **N0 (basic)** / **N1–N2** — neutrals (and isolate/format leftovers) take
//!   the surrounding strong direction when both sides agree (N1), otherwise the
//!   embedding direction (N2). Full bracket-pair resolution (N0) is **deferred**.
//! - **I1–I2** — implicit levels: at an even level, R→+1 and AN/EN→+2; at an odd
//!   level, L/EN/AN→+1.
//! - **L1 (partial)** — trailing whitespace and segment/paragraph separators are
//!   reset to the paragraph level (so a trailing space does not drag the line).
//! - **L2** — reorder: reverse each maximal run at level ≥ L, for L from the
//!   highest level down to the lowest odd level.
//!
//! Explicitly **deferred** (documented, not approximated): the explicit
//! embedding/override/isolate formatting characters (U+202A‥U+202E, U+2066‥
//! U+2069 — rules X1–X10), full bracket pairing (N0), and *intra-word*
//! reordering — the layout's atom is the whitespace-split word box, so a word
//! whose own characters span multiple resolved levels (e.g. Latin letters
//! immediately followed by Arabic-Indic digits with no space) is reordered as a
//! single unit at its dominant level rather than split mid-token. Every
//! whitespace token is in practice a single directional run, so this matches the
//! visual result for the overwhelming majority of content.

/// The bidirectional character type of a scalar, restricted to the classes the
/// non-explicit algorithm needs (UAX #9 Table 4). Explicit-formatting classes
/// (LRE/RLE/LRO/RLO/PDF/LRI/RLI/FSI/PDI) are folded into [`BidiClass::Bn`]
/// (boundary-neutral) since those code points are out of scope here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BidiClass {
    /// Strong left-to-right (L).
    L,
    /// Strong right-to-left (R) — Hebrew and other non-Arabic RTL.
    R,
    /// Strong right-to-left Arabic letter (AL).
    Al,
    /// European number (EN).
    En,
    /// European number separator (ES) — plus/minus.
    Es,
    /// European number terminator (ET) — `%`, `$`, `°`, `#`…
    Et,
    /// Arabic number (AN) — Arabic-Indic digits and the Arabic separators.
    An,
    /// Common number separator (CS) — `,`, `.`, `:`, `/`, NBSP…
    Cs,
    /// Non-spacing mark (NSM) — combining marks.
    Nsm,
    /// Boundary neutral (BN) — controls, zero-width, and folded explicit codes.
    Bn,
    /// Paragraph separator (B).
    B,
    /// Segment separator (S) — TAB.
    S,
    /// Whitespace (WS).
    Ws,
    /// Other neutral (ON) — most punctuation and symbols.
    On,
}

use BidiClass::*;

/// Classify a scalar into its [`BidiClass`].
///
/// This is a pragmatic mapping of the Unicode Bidi_Class property covering the
/// blocks the engine cares about (ASCII, Latin-1, the RTL scripts handled by the
/// shaper, and the most common neutrals/numbers). Code points outside the listed
/// ranges fall back to [`BidiClass::On`] (other neutral) or, for letters in the
/// general LTR range, to [`BidiClass::L`] — a safe default that never reorders
/// well-behaved Latin text.
fn bidi_class(c: char) -> BidiClass {
    let u = c as u32;
    match u {
        // ── Explicit-formatting and boundary-neutral controls (folded to BN) ──
        0x0000..=0x0008 | 0x000E..=0x001B | 0x007F..=0x0084 | 0x0086..=0x009F | 0x00AD => Bn,
        0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF => Bn,
        // ── Separators ──
        0x000A | 0x000D | 0x001C..=0x001E | 0x0085 | 0x2029 => B, // paragraph
        0x0009 | 0x000B | 0x001F => S,                            // segment (TAB)
        0x000C | 0x0020 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x205F | 0x3000 => Ws,
        // ── Numbers ──
        0x0030..=0x0039 => En, // ASCII digits
        0x00B2 | 0x00B3 | 0x00B9 | 0x2070..=0x2079 | 0x2080..=0x2089 => En, // super/subscript
        0x002B | 0x002D | 0x2212 => Es, // plus / hyphen-minus / minus
        0x0023..=0x0025 | 0x00A2..=0x00A5 | 0x00B0 | 0x066A => Et, // # $ % ¢£¤¥ ° ٪
        0x0660..=0x0669 | 0x066B | 0x066C | 0x06F0..=0x06F9 => An, // Arabic-Indic digits + seps
        0x002C | 0x002E | 0x002F | 0x003A | 0x00A0 => Cs, // , . / : NBSP
        // ── Strong RTL ──
        0x0608 | 0x060B | 0x060D | 0x061B..=0x064A | 0x066D..=0x066F | 0x0671..=0x06D5 => Al,
        0x06E5 | 0x06E6 | 0x06EE | 0x06EF | 0x06FA..=0x06FF => Al,
        0x0750..=0x077F | 0x08A0..=0x08FF => Al, // Arabic Supplement / Extended-A
        0xFB50..=0xFDCF | 0xFDF0..=0xFDFF | 0xFE70..=0xFEFF => Al, // Arabic presentation forms
        0x0590..=0x05FF | 0x07C0..=0x085F | 0xFB1D..=0xFB4F => R, // Hebrew / Thaana / NKo / Heb PF
        // ── Combining marks (non-spacing) ──
        0x0300..=0x036F | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 => Nsm,
        0x06E7 | 0x06E8 | 0x06EA..=0x06ED | 0x0711 | 0xFE20..=0xFE2F => Nsm,
        // ── Strong LTR letters we recognise explicitly ──
        0x0041..=0x005A | 0x0061..=0x007A => L, // ASCII letters
        0x00C0..=0x024F | 0x0370..=0x052F | 0x1E00..=0x1FFF => L, // Latin/Greek/Cyrillic
        0x2C60..=0x2C7F | 0xA720..=0xA7FF => L, // Latin Extended-C / -D
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF => {
            L
        }
        // ── Common ASCII neutrals (explicit, for clarity) ──
        0x0021 | 0x0022 | 0x0026..=0x002A | 0x003B..=0x0040 | 0x005B..=0x0060 | 0x007B..=0x007E => {
            On
        }
        // ── Default: letters elsewhere read LTR; everything else is neutral. ──
        _ if c.is_alphabetic() => L,
        _ => On,
    }
}

/// Compute the resolved embedding level of every `char` of `text`, given the
/// paragraph base level `base` (0 = LTR, 1 = RTL — rule P2/P3 supplied by the
/// caller from CSS `direction`). The returned vector is indexed by the `char`
/// position (not byte offset) and has one entry per scalar.
///
/// Applies the non-explicit rules W1–W7, N1–N2, I1–I2 and the trailing-reset of
/// L1, over a single paragraph (the engine breaks `<br>`/block boundaries before
/// this, so each call is one line of one paragraph).
fn resolve_levels(text: &str, base: u8) -> Vec<u8> {
    let mut types: Vec<BidiClass> = text.chars().map(bidi_class).collect();
    let n = types.len();
    if n == 0 {
        return Vec::new();
    }

    // ── W1: NSM takes the type of the previous character (sor ⇒ base dir). ──
    let sor = if base & 1 == 1 { R } else { L };
    for i in 0..n {
        if types[i] == Nsm {
            types[i] = if i == 0 { sor } else { types[i - 1] };
        }
    }

    // ── W2: EN after the last strong AL becomes AN. ──
    {
        let mut last_strong = sor;
        for t in types.iter_mut() {
            match *t {
                L | R | Al => last_strong = *t,
                En if last_strong == Al => *t = An,
                _ => {}
            }
        }
    }

    // ── W3: AL → R. ──
    for t in types.iter_mut() {
        if *t == Al {
            *t = R;
        }
    }

    // ── W4: a single ES between two EN ⇒ EN; a single CS between two equal
    //        numbers (EN/EN or AN/AN) ⇒ that number type. ──
    for i in 1..n.saturating_sub(1) {
        let prev = types[i - 1];
        let next = types[i + 1];
        match types[i] {
            Es if prev == En && next == En => types[i] = En,
            Cs if prev == En && next == En => types[i] = En,
            Cs if prev == An && next == An => types[i] = An,
            _ => {}
        }
    }

    // ── W5: a sequence of ET adjacent to EN takes the type EN. ──
    {
        let mut i = 0;
        while i < n {
            if types[i] == Et {
                let start = i;
                while i < n && types[i] == Et {
                    i += 1;
                }
                let before_en = start > 0 && types[start - 1] == En;
                let after_en = i < n && types[i] == En;
                if before_en || after_en {
                    for t in types.iter_mut().take(i).skip(start) {
                        *t = En;
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    // ── W6: otherwise, separators and terminators become ON. ──
    for t in types.iter_mut() {
        if matches!(*t, Es | Et | Cs) {
            *t = On;
        }
    }

    // ── W7: EN → L when the last strong type was L. ──
    {
        let mut last_strong = sor;
        for t in types.iter_mut() {
            match *t {
                L | R => last_strong = *t,
                En if last_strong == L => *t = L,
                _ => {}
            }
        }
    }

    // ── N1/N2: resolve neutral (and BN) sequences. Treat EN/AN as R for the
    //          purpose of choosing a neutral run's direction. ──
    let dir_of = |t: BidiClass| -> Option<BidiClass> {
        match t {
            L => Some(L),
            R => Some(R),
            En | An => Some(R),
            _ => None,
        }
    };
    let embedding = if base & 1 == 1 { R } else { L };
    {
        let is_neutral = |t: BidiClass| matches!(t, B | S | Ws | On | Bn);
        let mut i = 0;
        while i < n {
            if is_neutral(types[i]) {
                let start = i;
                while i < n && is_neutral(types[i]) {
                    i += 1;
                }
                // Strong directions flanking the neutral run (sor/eor = base).
                let before = if start == 0 {
                    embedding
                } else {
                    dir_of(types[start - 1]).unwrap_or(embedding)
                };
                let after = if i == n {
                    embedding
                } else {
                    dir_of(types[i]).unwrap_or(embedding)
                };
                let resolved = if before == after { before } else { embedding };
                for t in types.iter_mut().take(i).skip(start) {
                    *t = resolved;
                }
            } else {
                i += 1;
            }
        }
    }

    // ── I1/I2: implicit levels from the resolved types. ──
    let mut levels = vec![base; n];
    for (lvl, t) in levels.iter_mut().zip(types.iter()) {
        if base & 1 == 0 {
            // Even (LTR) embedding level.
            match t {
                R => *lvl = base + 1,
                En | An => *lvl = base + 2,
                _ => {}
            }
        } else {
            // Odd (RTL) embedding level.
            match t {
                L | En | An => *lvl = base + 1,
                _ => {}
            }
        }
    }

    // ── L1 (partial): reset trailing whitespace / separators to the base level
    //        (using the ORIGINAL classes), so a trailing space/tab does not pull
    //        the line. We recompute original classes for whitespace detection. ──
    let orig: Vec<BidiClass> = text.chars().map(bidi_class).collect();
    let mut k = n;
    while k > 0 {
        match orig[k - 1] {
            Ws | B | S | Bn => {
                levels[k - 1] = base;
                k -= 1;
            }
            _ => break,
        }
    }

    levels
}

/// Reverse, in place, each maximal sub-slice of `order` whose word's level
/// (`word_level[word_index]`) is `>= threshold` (one pass of rule L2). `order`
/// holds word indices; `word_level` is indexed by those word indices.
fn reverse_runs_at(order: &mut [usize], word_level: &[u8], threshold: u8) {
    let mut i = 0;
    let m = order.len();
    while i < m {
        if word_level[order[i]] >= threshold {
            let start = i;
            while i < m && word_level[order[i]] >= threshold {
                i += 1;
            }
            order[start..i].reverse();
        } else {
            i += 1;
        }
    }
}

/// The reordered **visual order of word boxes** for one already-broken line.
///
/// `word_chars` gives, for each word box in *logical* order, the slice of that
/// word's characters (the layout's atoms are whitespace-split tokens). `base` is
/// the paragraph embedding level from CSS `direction` (0 = LTR, 1 = RTL). The
/// result is a permutation of `0..word_chars.len()`: the order in which the word
/// boxes should be placed left-to-right on the page.
///
/// Each word is assigned a single level — the level resolved for its first
/// character that carries one (every whitespace token is, in practice, a single
/// directional run). Words are then reordered per rule **L2** over those
/// per-word levels. The within-word glyph order is untouched (the shaper already
/// produced it). For a pure-LTR line every level is 0, so the identity order is
/// returned and existing output is byte-identical.
pub fn reorder(word_chars: &[&str], base: u8) -> Vec<usize> {
    let count = word_chars.len();
    if count == 0 {
        return Vec::new();
    }

    // Build the line's logical character string (a single inter-word space
    // between tokens, mirroring the collapsed whitespace the layout placed) and
    // record where each word's characters start.
    let mut line = String::new();
    let mut starts = Vec::with_capacity(count);
    for (i, w) in word_chars.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        starts.push(line.chars().count());
        line.push_str(w);
    }

    let char_levels = resolve_levels(&line, base);
    let line_chars: Vec<char> = line.chars().collect();

    // Per-word level: every whitespace token is a single directional run, so the
    // word's level is the resolved level of its first *directionally meaningful*
    // character — the first strong letter or number, skipping leading neutrals
    // (punctuation/marks) that would otherwise borrow the surrounding direction.
    // A wholly-neutral token falls back to its first character's resolved level
    // (and ultimately `base`).
    let mut word_level = vec![base; count];
    for (i, w) in word_chars.iter().enumerate() {
        let start = starts[i];
        let len = w.chars().count();
        if len == 0 {
            continue;
        }
        word_level[i] = char_levels.get(start).copied().unwrap_or(base);
        for off in 0..len {
            let Some(&lv) = char_levels.get(start + off) else {
                break;
            };
            let strong = line_chars
                .get(start + off)
                .map(|&c| matches!(bidi_class(c), L | R | Al | En | An))
                .unwrap_or(false);
            word_level[i] = lv;
            if strong {
                break;
            }
        }
    }

    let mut order: Vec<usize> = (0..count).collect();
    let max_level = word_level.iter().copied().max().unwrap_or(base);
    // Lowest odd level (rule L2 reverses from the highest level down to the
    // lowest odd level inclusive).
    let mut lowest_odd = u8::MAX;
    for &lv in &word_level {
        if lv & 1 == 1 && lv < lowest_odd {
            lowest_odd = lv;
        }
    }
    if lowest_odd != u8::MAX {
        let mut threshold = max_level;
        while threshold >= lowest_odd {
            reverse_runs_at(&mut order, &word_level, threshold);
            if threshold == 0 {
                break;
            }
            threshold -= 1;
        }
    }

    order
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: resolved per-word visual order for a space-joined logical string.
    fn order_of(words: &[&str], base: u8) -> Vec<usize> {
        reorder(words, base)
    }

    #[test]
    fn classifies_core_types() {
        assert_eq!(bidi_class('A'), L);
        assert_eq!(bidi_class('a'), L);
        assert_eq!(bidi_class('א'), R); // Hebrew alef
        assert_eq!(bidi_class('ا'), Al); // Arabic alef
        assert_eq!(bidi_class('5'), En);
        assert_eq!(bidi_class('٥'), An); // Arabic-Indic five
        assert_eq!(bidi_class(' '), Ws);
        assert_eq!(bidi_class(','), Cs);
        assert_eq!(bidi_class('%'), Et);
        assert_eq!(bidi_class('+'), Es);
        assert_eq!(bidi_class('!'), On);
        assert_eq!(bidi_class('\n'), B);
        assert_eq!(bidi_class('\t'), S);
    }

    #[test]
    fn pure_ltr_is_identity() {
        // Pure-LTR text must return the identity permutation at base 0 — this is
        // what guarantees existing LTR output is byte-identical.
        assert_eq!(
            order_of(&["the", "quick", "brown", "fox"], 0),
            vec![0, 1, 2, 3]
        );
        assert_eq!(resolve_levels("hello world", 0), vec![0; 11]);
    }

    #[test]
    fn pure_rtl_reverses_whole_line() {
        // Three RTL (Hebrew) words at base level 1 are placed right-to-left:
        // the last logical word appears first (leftmost→ index reversed).
        let v = order_of(&["שלום", "עולם", "טוב"], 1);
        assert_eq!(v, vec![2, 1, 0]);
    }

    #[test]
    fn rtl_with_embedded_latin_word_keeps_latin_upright() {
        // Logical: RTL0 "ABC" RTL2  (Arabic … Latin … Arabic), base RTL.
        // Visual L2: the Arabic runs reverse but the Latin run "ABC" stays at its
        // own even level, reading left-to-right *inside* the reversed line.
        // Logical indices: 0=arabic 1=ABC 2=arabic.
        let v = order_of(&["عربية", "ABC", "عربي"], 1);
        // Whole line reverses (base odd) → [2,1,0]; the single Latin word, being
        // one box, sits between the two reversed Arabic words. Its box keeps its
        // own glyph order (handled by the shaper), so "ABC" reads correctly.
        assert_eq!(v, vec![2, 1, 0]);
    }

    #[test]
    fn rtl_with_two_embedded_latin_words_reorders_correctly() {
        // Logical: arabic, "ONE", "TWO", arabic (base RTL). The two Latin words
        // form a single even-level run; rule L2 reverses the line once (odd base)
        // and then reverses the Latin sub-run back, so "ONE" precedes "TWO"
        // visually (left→right) even though the surrounding Arabic is mirrored.
        let v = order_of(&["عربية", "ONE", "TWO", "عرب"], 1);
        // Expected visual (left→right): arabic3 , ONE , TWO , arabic0
        //   = logical indices [3, 1, 2, 0].
        assert_eq!(v, vec![3, 1, 2, 0]);
    }

    #[test]
    fn rtl_with_european_numbers_reads_ltr_inside() {
        // European numbers in an RTL line resolve to an even (LTR) sub-level
        // (I1: EN→base+2 at even base / →base+1 at odd base, but a number run is
        // ultimately level 2 here). Two number words keep ascending order.
        let v = order_of(&["عربية", "12", "34", "عرب"], 1);
        assert_eq!(v, vec![3, 1, 2, 0]);
    }

    #[test]
    fn ltr_base_with_embedded_hebrew_reverses_only_hebrew_run() {
        // Logical (base LTR): "the" HEB1 HEB2 "end". The two Hebrew words form an
        // odd-level run that reverses among themselves; the Latin words stay put.
        // Visual (left→right): the, HEB2, HEB1, end = [0, 2, 1, 3].
        let v = order_of(&["the", "שלום", "עולם", "end"], 0);
        assert_eq!(v, vec![0, 2, 1, 3]);
    }

    #[test]
    fn w7_en_after_l_becomes_l() {
        // "A5" — EN after strong L becomes L (W7), so the whole token is level 0.
        let levels = resolve_levels("A5", 0);
        assert_eq!(levels, vec![0, 0]);
    }

    #[test]
    fn w2_en_after_al_becomes_an() {
        // Arabic letter then ASCII digit: EN→AN (W2). At RTL base the AN sits at
        // level base+1 (=2 here) since I2 raises AN by 1 over the odd base? No —
        // AN at odd base goes to base+1. Verify the digit is not level 0 (LTR).
        let levels = resolve_levels("ا5", 1); // alef + '5'
                                              // alef→R→level 1; '5'→(W2)AN→ at odd base I2 gives base+1 = 2.
        assert_eq!(levels, vec![1, 2]);
    }

    #[test]
    fn n1_neutral_between_same_strongs_joins_them() {
        // Hebrew, '-', Hebrew at base LTR: the neutral '-' (→ON via W6) is flanked
        // by R on both sides ⇒ N1 makes it R ⇒ level 1, same as the letters.
        let levels = resolve_levels("א-ב", 0);
        assert_eq!(levels, vec![1, 1, 1]);
    }

    #[test]
    fn n2_neutral_between_opposite_strongs_takes_embedding() {
        // Latin, '!', Hebrew at base LTR: opposite strongs ⇒ N2 ⇒ embedding (L) ⇒
        // the neutral is level 0; only the Hebrew letter is level 1.
        let levels = resolve_levels("a!א", 0);
        assert_eq!(levels, vec![0, 0, 1]);
    }

    #[test]
    fn trailing_whitespace_resets_to_base() {
        // L1: a trailing space in an RTL line is reset to the base level (1), not
        // raised, so it never drags the visual order.
        let levels = resolve_levels("א ", 1);
        assert_eq!(levels, vec![1, 1]);
    }

    #[test]
    fn empty_and_single_word_are_trivial() {
        assert_eq!(reorder(&[], 1), Vec::<usize>::new());
        assert_eq!(reorder(&["x"], 0), vec![0]);
        assert_eq!(reorder(&["שלום"], 1), vec![0]);
    }
}
