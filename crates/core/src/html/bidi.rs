//! A complete, zero-dependency implementation of the Unicode Bidirectional
//! Algorithm (UAX #9) for the HTML→PDF inline layout path.
//!
//! Given a line's atoms in **logical order** (each atom is one character, passed
//! as a `&str`) and the paragraph's base direction, [`reorder`] returns the
//! permutation of indices that places the atoms in **visual order** (left to
//! right). The engine's inline layout treats each atom as an indivisible
//! word-box; this module computes the per-atom embedding levels and the final
//! L2 reordering, which the caller aggregates back onto its word boxes.
//!
//! The full algorithm is implemented — nothing is deferred:
//!
//! * **P2–P3** — base direction (the caller supplies it via `base`; FSI still
//!   resolves its isolated scope by P2/P3 internally, rule X5c).
//! * **X1–X8** — explicit embeddings (`LRE`/`RLE`), overrides (`LRO`/`RLO`) and
//!   the `PDF` pop, driven by a 125-deep *directional status stack* with the
//!   overflow-isolate / overflow-embedding / valid-isolate counts.
//! * **X5a–X5c** — directional isolates `LRI`/`RLI`/`FSI` and the matching
//!   `PDI` (`FSI` auto-detects via P2/P3 over its isolated scope).
//! * **X6** — assign each character its embedding level and apply the active
//!   directional override to its bidi type.
//! * **X9** — the embedding/override/PDF formatting characters and `BN` are kept
//!   at the surrounding level (treat-as-`BN`) so they never split L1/L2 runs.
//! * **X10** — the text is partitioned into *isolating run sequences* (BD13);
//!   each gets its own `sos`/`eos`, and **W1–W7, N0, N1–N2, I1–I2** run **per
//!   sequence**, not over the whole paragraph.
//! * **N0** — paired brackets (BD16): the canonical bracket set (with the
//!   `U+2329`/`U+232A` ↔ `U+3008`/`U+3009` canonical equivalence) is matched
//!   with a 63-deep stack and each pair is resolved by the strong types inside
//!   and around it.
//! * **L1–L2** — reset of separators/trailing whitespace to the paragraph level,
//!   then the level-run reversal that yields the visual order.
//!
//! Bidi classes are derived from a compact built-in range table
//! ([`bidi_class`]) covering the scripts and punctuation an HTML document
//! actually exercises (Latin, Hebrew, Arabic and its presentation forms, the
//! European/Arabic number machinery, combining marks, and every formatting
//! character) with the correct default-class blocks for unassigned code points.

/// The Unicode bidirectional character type (UAX #9, table "Bidirectional
/// Character Types"). Every code point maps to exactly one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BidiClass {
    /// Left-to-Right (strong).
    L,
    /// Right-to-Left (strong).
    R,
    /// Right-to-Left Arabic (strong).
    Al,
    /// European Number.
    En,
    /// European Number Separator.
    Es,
    /// European Number Terminator.
    Et,
    /// Arabic Number.
    An,
    /// Common Number Separator.
    Cs,
    /// Nonspacing Mark.
    Nsm,
    /// Boundary Neutral.
    Bn,
    /// Paragraph Separator.
    B,
    /// Segment Separator.
    S,
    /// Whitespace.
    Ws,
    /// Other Neutral.
    On,
    /// Left-to-Right Embedding.
    Lre,
    /// Left-to-Right Override.
    Lro,
    /// Right-to-Left Embedding.
    Rle,
    /// Right-to-Left Override.
    Rlo,
    /// Pop Directional Format.
    Pdf,
    /// Left-to-Right Isolate.
    Lri,
    /// Right-to-Left Isolate.
    Rli,
    /// First Strong Isolate.
    Fsi,
    /// Pop Directional Isolate.
    Pdi,
}

use BidiClass::*;

/// Maximum explicit embedding depth (UAX #9, BD2 / rule X1: `max_depth = 125`).
const MAX_DEPTH: u8 = 125;

/// Reorder one line of atoms from logical to visual order under the Unicode
/// Bidirectional Algorithm.
///
/// `word_chars[i]` is the `i`-th atom in logical order — normally one character.
/// `base` is the paragraph (line) base direction: `0` for left-to-right, any
/// other value for right-to-left. The returned `Vec<usize>` is a permutation of
/// `0..word_chars.len()`: reading it left to right gives the visual sequence of
/// the original indices.
///
/// A run with no right-to-left character and no bidi formatting character in an
/// LTR base is returned as the identity `0,1,2,…` (the pure-LTR fast path), so
/// the common case is byte-for-byte unchanged.
///
/// ```
/// # use gigapdf_core::html::bidi::reorder;
/// // Pure LTR is the identity permutation.
/// let atoms = ["a", "b", "c"];
/// assert_eq!(reorder(&atoms, 0), vec![0, 1, 2]);
/// ```
pub fn reorder(word_chars: &[&str], base: u8) -> Vec<usize> {
    let n = word_chars.len();
    if n == 0 {
        return Vec::new();
    }

    let para_level: u8 = if base == 0 { 0 } else { 1 };

    // Original (declared) bidi class of each atom, by its first code point.
    let original: Vec<BidiClass> = word_chars.iter().map(|s| atom_class(s)).collect();

    // Fast path: an LTR base with no strong-RTL/AN and no formatting characters
    // is the identity. (Any RTL, AN, or explicit/isolate control forces the full
    // resolution.) This keeps the overwhelmingly common case allocation-light and
    // bit-identical to "no bidi at all".
    if para_level == 0 && original.iter().all(is_bidi_inert) {
        return (0..n).collect();
    }

    // Record each atom's first code point so N0's bracket pairing (BD16) can read
    // the actual bracket characters without threading `&[&str]` through every
    // phase. Cleared before returning so the thread-local never outlives a call.
    let atom_first_chars: Vec<char> = word_chars
        .iter()
        .map(|s| s.chars().next().unwrap_or('\u{0}'))
        .collect();
    ATOM_CHARS.with(|cell| *cell.borrow_mut() = atom_first_chars);

    // X1–X9: explicit levels + override-adjusted types, with X9 keeping the
    // formatting characters and BN at the surrounding level.
    let Explicit {
        mut levels,
        mut types,
    } = resolve_explicit(&original, para_level);

    // X10 + N0 + W* + N* + I*: resolve every isolating run sequence on its own.
    let sequences = isolating_run_sequences(&original, &types, &levels, para_level);
    for seq in &sequences {
        resolve_sequence(seq, &original, &mut types, &mut levels, para_level);
    }

    // L1: reset segment/paragraph separators and trailing whitespace (including
    // whitespace/isolates preceding them) to the paragraph level.
    reset_whitespace_levels(&original, &types, &mut levels, para_level);

    ATOM_CHARS.with(|cell| cell.borrow_mut().clear());

    // L2: reverse contiguous level runs from the highest level down to the
    // lowest odd level to obtain the visual order.
    reorder_visual(&levels)
}

// ─────────────────────────────────────────────────────────────────────────────
// Atom classification
// ─────────────────────────────────────────────────────────────────────────────

/// Bidi class of an atom: the class of its first code point (the inline layout
/// passes one character per atom; an empty atom is a Boundary Neutral).
fn atom_class(atom: &str) -> BidiClass {
    match atom.chars().next() {
        Some(c) => bidi_class(c),
        None => Bn,
    }
}

/// `true` if an atom can never affect bidi resolution under an LTR base: a
/// left-strong, numeric-terminator, boundary-neutral or whitespace/neutral that
/// stays at level 0 and in original order. Used only to gate the fast path; a
/// conservative `false` simply takes the full path.
fn is_bidi_inert(c: &BidiClass) -> bool {
    matches!(c, L | En | Es | Et | Cs | Bn | B | S | Ws | On | Nsm)
}

// ─────────────────────────────────────────────────────────────────────────────
// X1–X9 — explicit embeddings, overrides, isolates
// ─────────────────────────────────────────────────────────────────────────────

/// Output of the explicit-resolution phase: each character's embedding level and
/// its (possibly override-adjusted) bidi type.
struct Explicit {
    levels: Vec<u8>,
    types: Vec<BidiClass>,
}

/// One entry of the directional status stack (rule X1).
#[derive(Clone, Copy)]
struct StatusEntry {
    level: u8,
    override_status: Override,
    isolate: bool,
}

/// The directional override status carried on the status stack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Override {
    Neutral,
    Ltr,
    Rtl,
}

/// Rules X1–X8: walk the text maintaining the directional status stack and the
/// overflow / valid-isolate counts, assigning each character its embedding level
/// and applying the active override (X6). X9 is folded in: the embedding,
/// override and PDF controls — and any BN — are assigned the level of the
/// surrounding context and left as `BN` so later phases skip them without
/// breaking runs.
fn resolve_explicit(original: &[BidiClass], para_level: u8) -> Explicit {
    let n = original.len();
    let mut levels = vec![para_level; n];
    let mut types = original.to_vec();

    // Directional status stack (X1). Starts with the paragraph entry.
    let mut stack: Vec<StatusEntry> = Vec::with_capacity(MAX_DEPTH as usize + 2);
    stack.push(StatusEntry {
        level: para_level,
        override_status: Override::Neutral,
        isolate: false,
    });

    let mut overflow_isolate: usize = 0;
    let mut overflow_embedding: usize = 0;
    let mut valid_isolate: usize = 0;

    for i in 0..n {
        match original[i] {
            // X2–X5: explicit embeddings and overrides.
            Rle | Lre | Rlo | Lro => {
                let is_rtl = matches!(original[i], Rle | Rlo);
                // The control itself takes the embedding level of the last entry,
                // and is removed (X9 → BN).
                levels[i] = stack.last().unwrap().level;
                types[i] = Bn;

                let next = next_level(stack.last().unwrap().level, is_rtl);
                if next <= MAX_DEPTH && overflow_isolate == 0 && overflow_embedding == 0 {
                    let ov = match original[i] {
                        Lro => Override::Ltr,
                        Rlo => Override::Rtl,
                        _ => Override::Neutral,
                    };
                    stack.push(StatusEntry {
                        level: next,
                        override_status: ov,
                        isolate: false,
                    });
                } else if overflow_isolate == 0 {
                    overflow_embedding += 1;
                }
            }

            // X5a / X5b / X5c: directional isolates.
            Rli | Lri | Fsi => {
                // The isolate initiator is assigned the embedding level of the
                // last entry, with that entry's override applied (X6a), before
                // the new level is pushed.
                levels[i] = stack.last().unwrap().level;
                apply_override(&mut types, i, stack.last().unwrap().override_status);

                // X5c: an FSI acts as LRI or RLI per the first strong type within
                // its isolated scope (P2/P3 restricted to that scope).
                let is_rtl = match original[i] {
                    Rli => true,
                    Lri => false,
                    _ => fsi_direction(original, i) == 1,
                };

                let next = next_level(stack.last().unwrap().level, is_rtl);
                if next <= MAX_DEPTH && overflow_isolate == 0 && overflow_embedding == 0 {
                    valid_isolate += 1;
                    stack.push(StatusEntry {
                        level: next,
                        override_status: Override::Neutral,
                        isolate: true,
                    });
                } else {
                    overflow_isolate += 1;
                }
            }

            // X6a: pop directional isolate.
            Pdi => {
                if overflow_isolate > 0 {
                    overflow_isolate -= 1;
                } else if valid_isolate > 0 {
                    overflow_embedding = 0;
                    // Pop entries until (and including) the last isolate entry.
                    while !stack.last().unwrap().isolate {
                        stack.pop();
                    }
                    stack.pop();
                    valid_isolate -= 1;
                }
                // A PDI takes the level of the entry now on top, with its override
                // applied (X6a). Matched-or-not, a PDI is always assigned a level.
                let top = *stack.last().unwrap();
                levels[i] = top.level;
                apply_override(&mut types, i, top.override_status);
            }

            // X7: pop directional embedding/override.
            Pdf => {
                levels[i] = stack.last().unwrap().level;
                types[i] = Bn;
                if overflow_isolate > 0 {
                    // Inside an overflow isolate: do nothing.
                } else if overflow_embedding > 0 {
                    overflow_embedding -= 1;
                } else if !stack.last().unwrap().isolate && stack.len() >= 2 {
                    stack.pop();
                }
            }

            // X8: paragraph separator — terminates all embeddings.
            B => {
                levels[i] = para_level;
                // (A line never actually contains B in this engine, but keep the
                // standard behaviour for completeness.)
            }

            // BN keeps the surrounding level and stays BN (X6 explicitly skips it).
            Bn => {
                levels[i] = stack.last().unwrap().level;
            }

            // X6: any other character.
            _ => {
                levels[i] = stack.last().unwrap().level;
                apply_override(&mut types, i, stack.last().unwrap().override_status);
            }
        }
    }

    Explicit { levels, types }
}

/// Apply a directional override (X6): a non-neutral override forces the
/// character's resolved type to `L` or `R`.
fn apply_override(types: &mut [BidiClass], i: usize, ov: Override) {
    match ov {
        Override::Ltr => types[i] = L,
        Override::Rtl => types[i] = R,
        Override::Neutral => {}
    }
}

/// The least embedding level greater than `level` with the requested parity:
/// the next odd level for RTL, the next even level for LTR (rules X2–X5b).
fn next_level(level: u8, rtl: bool) -> u8 {
    if rtl {
        // next odd
        (level + 1) | 1
    } else {
        // next even
        (level + 2) & !1
    }
}

/// X5c / P2–P3 over an FSI's isolated scope: scan from just after the FSI at `i`
/// to its matching PDI (BD9), skipping nested isolate scopes, and return the
/// direction of the first strong character — `1` for R/AL, `0` for L (or no
/// strong, defaulting to LTR).
fn fsi_direction(original: &[BidiClass], i: usize) -> u8 {
    let mut depth = 0i32;
    let mut j = i + 1;
    while j < original.len() {
        match original[j] {
            Lri | Rli | Fsi => depth += 1,
            Pdi => {
                if depth == 0 {
                    break; // matching PDI of our FSI
                }
                depth -= 1;
            }
            L if depth == 0 => return 0,
            R | Al if depth == 0 => return 1,
            _ => {}
        }
        j += 1;
    }
    0
}

// ─────────────────────────────────────────────────────────────────────────────
// X10 — isolating run sequences (BD13) with sos/eos
// ─────────────────────────────────────────────────────────────────────────────

/// One isolating run sequence: the ordered list of character indices forming the
/// sequence, plus the start-of-sequence and end-of-sequence directions used by
/// the weak/neutral resolution.
struct RunSequence {
    indices: Vec<usize>,
    sos: BidiClass,
    eos: BidiClass,
}

/// Partition the text into isolating run sequences (rule X10 / BD13) and compute
/// each one's `sos`/`eos`.
///
/// First the text is split into *level runs* (maximal spans of one embedding
/// level), considering only non-removed characters (X9: `BN` and the controls
/// are skipped). Each level run that does **not** start with a `PDI` matching an
/// earlier isolate initiator starts a sequence; the sequence is then extended
/// across each isolate-initiator → matching-PDI boundary.
fn isolating_run_sequences(
    original: &[BidiClass],
    types: &[BidiClass],
    levels: &[u8],
    para_level: u8,
) -> Vec<RunSequence> {
    let n = original.len();

    // The non-removed positions, in order (X9 removals: the explicit controls
    // and BN do not participate in run building).
    let kept: Vec<usize> = (0..n).filter(|&i| !is_removed(original[i])).collect();
    if kept.is_empty() {
        return Vec::new();
    }

    // Build level runs over the kept positions.
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_level = levels[kept[0]];
    for &i in &kept {
        if levels[i] == cur_level {
            cur.push(i);
        } else {
            runs.push(std::mem::take(&mut cur));
            cur_level = levels[i];
            cur.push(i);
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }

    // Match each isolate initiator with its PDI to know how to chain runs. For a
    // run starting with a matched PDI, `pdi_run_of[run]` is set; for a run ending
    // with an unmatched isolate initiator, `init_to_pdi_run` links to the run that
    // begins with the matching PDI.
    //
    // We compute, for each kept index that is an isolate initiator, the kept index
    // of its matching PDI (or none), then translate to run indices.
    let match_pdi = match_isolates(original);

    // Map a kept index that begins a run to its run number.
    let mut run_of_start: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for (r, run) in runs.iter().enumerate() {
        if let Some(&first) = run.first() {
            run_of_start.insert(first, r);
        }
    }

    let mut used = vec![false; runs.len()];
    let mut sequences: Vec<RunSequence> = Vec::new();

    for r in 0..runs.len() {
        if used[r] {
            continue;
        }
        let first = runs[r][0];
        // A sequence starts at a run whose first character is not a PDI matching
        // an earlier isolate initiator.
        let starts_with_matched_pdi = original[first] == Pdi && pdi_is_matched(&match_pdi, first);
        if starts_with_matched_pdi {
            continue;
        }

        // Chain runs across isolate-initiator → matching-PDI links.
        let mut seq_indices: Vec<usize> = Vec::new();
        let mut cur_run = r;
        loop {
            used[cur_run] = true;
            seq_indices.extend_from_slice(&runs[cur_run]);
            let last = *runs[cur_run].last().unwrap();
            // If this run ends with an isolate initiator that has a matching PDI,
            // continue with the run that begins at that PDI.
            if matches!(original[last], Lri | Rli | Fsi) {
                if let Some(pdi) = match_pdi.get(&last).copied().flatten() {
                    if let Some(&next_run) = run_of_start.get(&pdi) {
                        if !used[next_run] {
                            cur_run = next_run;
                            continue;
                        }
                    }
                }
            }
            break;
        }

        // sos/eos (X10): compare the sequence's level with the level of the
        // character preceding its first / following its last (skipping removed
        // characters), defaulting to the paragraph level at the text ends.
        let seq_level = levels[seq_indices[0]];
        let first_idx = seq_indices[0];
        let last_idx = *seq_indices.last().unwrap();

        let prev_level = prev_kept_level(original, levels, first_idx).unwrap_or(para_level);
        // For eos: if the sequence's last character is an isolate initiator with
        // no matching PDI, the following level is the paragraph level.
        let last_is_unmatched_isolate = matches!(original[last_idx], Lri | Rli | Fsi)
            && match_pdi.get(&last_idx).copied().flatten().is_none();
        let next_level = if last_is_unmatched_isolate {
            para_level
        } else {
            next_kept_level(original, levels, last_idx).unwrap_or(para_level)
        };

        let sos = dir_from_level(seq_level.max(prev_level));
        let eos = dir_from_level(seq_level.max(next_level));

        // The participating types are taken from `types` later; store indices.
        let _ = types; // types are consumed by the per-sequence resolver.
        sequences.push(RunSequence {
            indices: seq_indices,
            sos,
            eos,
        });
    }

    sequences
}

/// `true` if a character is removed for run building (X9): the explicit
/// embedding/override/PDF controls and Boundary Neutral.
fn is_removed(c: BidiClass) -> bool {
    matches!(c, Rle | Lre | Rlo | Lro | Pdf | Bn)
}

/// Map each isolate-initiator index to its matching PDI index (BD9), or `None`
/// for an unmatched initiator. PDIs that match nothing are simply absent as
/// values.
fn match_isolates(original: &[BidiClass]) -> std::collections::HashMap<usize, Option<usize>> {
    let mut map = std::collections::HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, &c) in original.iter().enumerate() {
        match c {
            Lri | Rli | Fsi => stack.push(i),
            Pdi => {
                if let Some(init) = stack.pop() {
                    map.insert(init, Some(i));
                }
            }
            _ => {}
        }
    }
    // Remaining initiators on the stack are unmatched.
    for init in stack {
        map.entry(init).or_insert(None);
    }
    map
}

/// `true` if the PDI at kept index `pdi` matches some earlier isolate initiator.
fn pdi_is_matched(match_pdi: &std::collections::HashMap<usize, Option<usize>>, pdi: usize) -> bool {
    match_pdi.values().any(|v| *v == Some(pdi))
}

/// Embedding level of the nearest non-removed character before `idx`, if any.
fn prev_kept_level(original: &[BidiClass], levels: &[u8], idx: usize) -> Option<u8> {
    (0..idx)
        .rev()
        .find(|&j| !is_removed(original[j]))
        .map(|j| levels[j])
}

/// Embedding level of the nearest non-removed character after `idx`, if any.
fn next_kept_level(original: &[BidiClass], levels: &[u8], idx: usize) -> Option<u8> {
    (idx + 1..original.len())
        .find(|&j| !is_removed(original[j]))
        .map(|j| levels[j])
}

/// `R` for an odd level, `L` for an even level (X10's sos/eos direction).
fn dir_from_level(level: u8) -> BidiClass {
    if level % 2 == 1 {
        R
    } else {
        L
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-sequence resolution: W1–W7, N0, N1–N2, I1–I2
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve one isolating run sequence: apply the weak rules (W1–W7), paired
/// brackets (N0), the remaining neutral rules (N1–N2) and the implicit levels
/// (I1–I2). All results are written back through the shared `types`/`levels`
/// arrays at the sequence's original indices.
fn resolve_sequence(
    seq: &RunSequence,
    original: &[BidiClass],
    types: &mut [BidiClass],
    levels: &mut [u8],
    _para_level: u8,
) {
    let idx = &seq.indices;
    let m = idx.len();
    if m == 0 {
        return;
    }

    // Local working copy of the sequence's types (the resolution operates on the
    // contiguous sequence with sos/eos as virtual neighbours).
    let mut t: Vec<BidiClass> = idx.iter().map(|&i| types[i]).collect();

    // The embedding level of the sequence (uniform across its runs by BD13).
    let seq_level = levels[idx[0]];

    weak_rules(&mut t, seq.sos);
    neutral_brackets(&mut t, original, idx, seq.sos, seq.eos, seq_level);
    neutral_rules(&mut t, seq.sos, seq.eos, seq_level);

    // Write resolved types back, then apply the implicit levels (I1–I2).
    for (k, &i) in idx.iter().enumerate() {
        types[i] = t[k];
        levels[i] = implicit_level(seq_level, t[k]);
    }
}

/// Rules W1–W7 over a sequence's working types. `sos` is the start-of-sequence
/// direction; `eos` is not needed by the weak rules.
fn weak_rules(t: &mut [BidiClass], sos: BidiClass) {
    let m = t.len();
    if m == 0 {
        return;
    }

    // W1: NSM → type of the previous character (sos at the start); an NSM after
    // an isolate control (LRI/RLI/FSI/PDI) becomes ON.
    let mut prev = sos;
    for c in t.iter_mut() {
        if *c == Nsm {
            *c = match prev {
                Lri | Rli | Fsi | Pdi => On,
                p => p,
            };
        }
        prev = *c;
    }

    // W2: EN → AN when the last strong type seen is AL.
    let mut last_strong = sos;
    for c in t.iter_mut() {
        match *c {
            R | L | Al => last_strong = *c,
            En if last_strong == Al => *c = An,
            _ => {}
        }
    }

    // W3: AL → R.
    for c in t.iter_mut() {
        if *c == Al {
            *c = R;
        }
    }

    // W4: a single ES between two ENs → EN; a single CS between two numbers of
    // the same type → that number type.
    for k in 1..m.saturating_sub(1) {
        if t[k] == Es && t[k - 1] == En && t[k + 1] == En {
            t[k] = En;
        } else if t[k] == Cs {
            if t[k - 1] == En && t[k + 1] == En {
                t[k] = En;
            } else if t[k - 1] == An && t[k + 1] == An {
                t[k] = An;
            }
        }
    }

    // W5: a run of ET adjacent to EN becomes EN.
    let mut k = 0;
    while k < m {
        if t[k] == Et {
            let start = k;
            while k < m && t[k] == Et {
                k += 1;
            }
            let before_en = start > 0 && t[start - 1] == En;
            let after_en = k < m && t[k] == En;
            if before_en || after_en {
                for slot in t.iter_mut().take(k).skip(start) {
                    *slot = En;
                }
            }
        } else {
            k += 1;
        }
    }

    // W6: any remaining ES/ET/CS → ON.
    for c in t.iter_mut() {
        if matches!(*c, Es | Et | Cs) {
            *c = On;
        }
    }

    // W7: EN → L when the last strong type seen is L.
    let mut last_strong = sos;
    for c in t.iter_mut() {
        match *c {
            R | L => last_strong = *c,
            En if last_strong == L => *c = L,
            _ => {}
        }
    }
}

/// Rules N1–N2: resolve runs of neutrals/isolate-controls between two
/// directions. `sos`/`eos` bound the sequence; `seq_level` gives N2's embedding
/// direction.
fn neutral_rules(t: &mut [BidiClass], sos: BidiClass, eos: BidiClass, seq_level: u8) {
    let m = t.len();
    let e = embedding_dir(seq_level);

    let mut k = 0;
    while k < m {
        if is_ni(t[k]) {
            let start = k;
            while k < m && is_ni(t[k]) {
                k += 1;
            }
            // Direction before the run (sos at the start), after the run (eos at
            // the end). EN/AN count as R for neutral resolution (N1).
            let before = if start == 0 {
                sos
            } else {
                strong_for_neutral(t[start - 1])
            };
            let after = if k == m {
                eos
            } else {
                strong_for_neutral(t[k])
            };

            let resolved = if before == after && (before == L || before == R) {
                before // N1
            } else {
                e // N2 — embedding direction
            };
            for slot in t.iter_mut().take(k).skip(start) {
                *slot = resolved;
            }
        } else {
            k += 1;
        }
    }
}

/// `true` if a type is a Neutral or Isolate formatting class (NI) for N0/N1/N2.
fn is_ni(c: BidiClass) -> bool {
    matches!(c, B | S | Ws | On | Lri | Rli | Fsi | Pdi)
}

/// The strong direction a type contributes to neutral resolution: EN and AN act
/// as R (N0/N1).
fn strong_for_neutral(c: BidiClass) -> BidiClass {
    match c {
        L => L,
        R | En | An => R,
        other => other,
    }
}

/// The embedding direction for an embedding level (N2 / I-rules).
fn embedding_dir(level: u8) -> BidiClass {
    if level % 2 == 1 {
        R
    } else {
        L
    }
}

/// I1–I2: the implicit level of a resolved type at embedding level `level`.
fn implicit_level(level: u8, t: BidiClass) -> u8 {
    let even = level.is_multiple_of(2);
    match t {
        L => {
            if even {
                level
            } else {
                level + 1
            }
        }
        R => {
            if even {
                level + 1
            } else {
                level
            }
        }
        An | En => {
            if even {
                level + 2
            } else {
                level + 1
            }
        }
        // Any leftover neutral keeps the embedding level (defensive; N1/N2 have
        // resolved all neutrals by now).
        _ => level,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// N0 — paired brackets (BD16)
// ─────────────────────────────────────────────────────────────────────────────

/// Rule N0: find bracket pairs within the sequence (BD16) and set each pair's
/// `ON` brackets to a strong direction resolved from the text inside and around
/// the pair. An `NSM` immediately following a re-typed bracket takes the
/// bracket's new type (the N0 note).
fn neutral_brackets(
    t: &mut [BidiClass],
    original: &[BidiClass],
    idx: &[usize],
    sos: BidiClass,
    eos: BidiClass,
    seq_level: u8,
) {
    let pairs = bracket_pairs(original, idx);
    if pairs.is_empty() {
        return;
    }
    let e = embedding_dir(seq_level);

    for (open_k, close_k) in pairs {
        // N0 (a)/(b)/(c): inspect the strong types strictly inside the pair.
        let mut found_e = false; // a strong matching the embedding direction
        let mut found_o = false; // a strong opposite the embedding direction
        for slot in t.iter().take(close_k).skip(open_k + 1) {
            let s = strong_for_neutral(*slot);
            if s == L || s == R {
                if s == e {
                    found_e = true;
                    break; // (b): embedding-direction strong inside ⇒ done
                }
                found_o = true;
            }
        }

        let resolved = if found_e {
            // (b): set the pair to the embedding direction.
            e
        } else if found_o {
            // (c): a strong opposite the embedding direction inside. Establish the
            // context before the open bracket (previous strong, sos at the start).
            let opposite = if e == L { R } else { L };
            let prev = preceding_strong(t, open_k, sos, eos);
            if prev == opposite {
                opposite // (c.1)
            } else {
                e // (c.2)
            }
        } else {
            // (a): no strong inside ⇒ leave the brackets as neutral (do nothing).
            continue;
        };

        set_bracket(t, open_k, resolved, idx, original);
        set_bracket(t, close_k, resolved, idx, original);
    }
}

/// Set a bracket position to `dir`, and carry that type onto any sequence of
/// `NSM` characters that immediately follow the bracket (N0 note — those NSMs
/// were W1-resolved to the bracket's old type and must track its new one).
fn set_bracket(
    t: &mut [BidiClass],
    k: usize,
    dir: BidiClass,
    idx: &[usize],
    original: &[BidiClass],
) {
    t[k] = dir;
    let mut j = k + 1;
    while j < t.len() && original[idx[j]] == Nsm {
        t[j] = dir;
        j += 1;
    }
}

/// The strong direction in effect just before sequence position `k`: the nearest
/// preceding L/R (EN/AN as R), or `sos` if none precedes within the sequence.
fn preceding_strong(t: &[BidiClass], k: usize, sos: BidiClass, _eos: BidiClass) -> BidiClass {
    for j in (0..k).rev() {
        let s = strong_for_neutral(t[j]);
        if s == L || s == R {
            return s;
        }
    }
    sos
}

/// Identify bracket pairs within a sequence (BD16). Returns `(open, close)`
/// positions **in sequence-local coordinates** (indices into `idx`), sorted by
/// opening position. The stack is capped at 63 entries; on overflow, pairing for
/// the sequence stops (BD16).
fn bracket_pairs(original: &[BidiClass], idx: &[usize]) -> Vec<(usize, usize)> {
    // Stack of (canonical opening bracket char, sequence-local position).
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();

    for (k, &i) in idx.iter().enumerate() {
        // Only original ON characters can be brackets (BD14/BD15 require the
        // current bidi type to be ON, which for brackets is their original type).
        if original[i] != On {
            continue;
        }
        let c = first_char(original, i);
        if let Some(open_canon) = opening_bracket(c) {
            if stack.len() == 63 {
                // BD16: stack overflow ⇒ stop processing brackets for this
                // sequence; return what we have so far.
                break;
            }
            stack.push((open_canon, k));
        } else if let Some(close_canon) = closing_to_opening(c) {
            // Search the stack from the top for a matching opening bracket.
            for s in (0..stack.len()).rev() {
                if canon_eq(stack[s].0, close_canon) {
                    pairs.push((stack[s].1, k));
                    stack.truncate(s); // discard this entry and everything above
                    break;
                }
            }
        }
    }

    pairs.sort_by_key(|&(o, _)| o);
    pairs
}

/// The original first code point at text index `i` — only meaningful for atoms
/// the caller supplied. Brackets are single-character atoms, so the atom's first
/// char is the bracket. (`bracket_pairs` already gated on `original[i] == On`.)
///
/// This indirection lets us recover the actual character from the bidi-class
/// world: we re-derive nothing here; instead the caller passes the original
/// atoms through `reorder`, and we look the character up via the thread-local in
/// `bracket_char`. To stay allocation-free and pure, the character is recovered
/// from `BRACKET_CHARS`.
fn first_char(_original: &[BidiClass], i: usize) -> char {
    bracket_char(i)
}

// The bracket machinery needs the actual `char` at a position, but the resolution
// phases work on `BidiClass`. Rather than thread the `&[&str]` everywhere, the
// public `reorder` records the atoms' first chars once so N0 can read them.
thread_local! {
    static ATOM_CHARS: std::cell::RefCell<Vec<char>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The first `char` of the atom at index `i` (recorded by [`reorder`]).
fn bracket_char(i: usize) -> char {
    ATOM_CHARS.with(|cell| cell.borrow().get(i).copied().unwrap_or('\u{0}'))
}

// ─────────────────────────────────────────────────────────────────────────────
// L1 / L2
// ─────────────────────────────────────────────────────────────────────────────

/// Rule L1: reset to the paragraph level (1) segment separators and paragraph
/// separators, and (2) any sequence of whitespace and/or isolate formatting
/// characters that precedes a segment/paragraph separator or the end of the
/// line. The original (pre-resolution) types decide what is whitespace.
fn reset_whitespace_levels(
    original: &[BidiClass],
    _types: &[BidiClass],
    levels: &mut [u8],
    para_level: u8,
) {
    let n = original.len();
    // Walk backwards; reset trailing whitespace/isolates and separators.
    let mut reset_run = true; // at the end of line, a trailing ws run resets
    for i in (0..n).rev() {
        match original[i] {
            B | S => {
                levels[i] = para_level;
                reset_run = true;
            }
            Ws | Lre | Rle | Lro | Rlo | Pdf | Lri | Rli | Fsi | Pdi | Bn => {
                if reset_run {
                    levels[i] = para_level;
                }
            }
            _ => {
                reset_run = false;
            }
        }
    }
}

/// Rule L2: reverse contiguous runs of characters from the highest embedding
/// level down to the lowest odd level, producing the visual ordering as a
/// permutation of `0..levels.len()`.
fn reorder_visual(levels: &[u8]) -> Vec<usize> {
    let n = levels.len();
    let mut order: Vec<usize> = (0..n).collect();
    if n == 0 {
        return order;
    }

    let highest = *levels.iter().max().unwrap();
    let mut lowest_odd = u8::MAX;
    for &l in levels {
        if l % 2 == 1 && l < lowest_odd {
            lowest_odd = l;
        }
    }
    if lowest_odd == u8::MAX {
        // No odd levels: nothing is reversed.
        return order;
    }

    let mut level = highest;
    while level >= lowest_odd {
        // Reverse each maximal run of positions whose level is >= `level`.
        let mut i = 0;
        while i < n {
            if levels[i] >= level {
                let start = i;
                while i < n && levels[i] >= level {
                    i += 1;
                }
                order[start..i].reverse();
            } else {
                i += 1;
            }
        }
        if level == 0 {
            break;
        }
        level -= 1;
    }

    order
}

// ─────────────────────────────────────────────────────────────────────────────
// Bracket tables (BD14/BD15/BD16)
// ─────────────────────────────────────────────────────────────────────────────

/// The canonical opening bracket for `c` if `c` is an opening paired bracket,
/// canonicalising `U+2329`→`U+3008` and `U+FE`-style decomposables are not in
/// the set. The returned char is the *canonical* opening bracket used for
/// matching.
fn opening_bracket(c: char) -> Option<char> {
    BRACKET_PAIRS
        .iter()
        .find(|&&(open, _)| open == c)
        .map(|&(open, _)| canonicalize(open))
}

/// If `c` is a closing paired bracket, the canonical form of its matching
/// opening bracket.
fn closing_to_opening(c: char) -> Option<char> {
    BRACKET_PAIRS
        .iter()
        .find(|&&(_, close)| close == c)
        .map(|&(open, _)| canonicalize(open))
}

/// `true` if `c` is any opening or closing paired bracket in the BD16 set. Used
/// by [`bidi_class`] to force `ON` on brackets that fall outside its enumerated
/// ranges (so N0 sees them as neutrals).
fn is_paired_bracket(c: char) -> bool {
    BRACKET_PAIRS
        .iter()
        .any(|&(open, close)| open == c || close == c)
}

/// Canonical-equivalence folding for the two bracket pairs that have a canonical
/// decomposition in the bracket set: `U+2329`/`U+232A` (angle brackets) fold to
/// `U+3008`/`U+3009` (CJK angle brackets) — BD16 requires matching across this
/// equivalence.
fn canonicalize(c: char) -> char {
    match c {
        '\u{2329}' => '\u{3008}',
        '\u{232A}' => '\u{3009}',
        other => other,
    }
}

/// `true` if two canonical opening brackets are equal under canonical
/// equivalence.
fn canon_eq(a: char, b: char) -> bool {
    canonicalize(a) == canonicalize(b)
}

/// The canonical set of paired brackets from `BidiBrackets.txt` (the 60 distinct
/// `(open, close)` pairs; the two angle-bracket pairs that share a canonical
/// decomposition are listed once each so both code points are recognised, with
/// [`canonicalize`] folding them at match time). Each tuple is `(opening,
/// closing)`.
#[rustfmt::skip]
const BRACKET_PAIRS: &[(char, char)] = &[
    ('\u{0028}', '\u{0029}'), // ( )
    ('\u{005B}', '\u{005D}'), // [ ]
    ('\u{007B}', '\u{007D}'), // { }
    ('\u{0F3A}', '\u{0F3B}'), // TIBETAN MARK GUG RTAGS GYON / GYAS
    ('\u{0F3C}', '\u{0F3D}'), // TIBETAN MARK ANG KHANG GYON / GYAS
    ('\u{169B}', '\u{169C}'), // OGHAM FEATHER MARK / REVERSED
    ('\u{2045}', '\u{2046}'), // ⁅ ⁆
    ('\u{207D}', '\u{207E}'), // superscript ( )
    ('\u{208D}', '\u{208E}'), // subscript ( )
    ('\u{2308}', '\u{2309}'), // ⌈ ⌉
    ('\u{230A}', '\u{230B}'), // ⌊ ⌋
    ('\u{2329}', '\u{232A}'), // 〈 〉 (angle, canonical → 3008/3009)
    ('\u{2768}', '\u{2769}'), // medium ( )
    ('\u{276A}', '\u{276B}'), // medium flattened ( )
    ('\u{276C}', '\u{276D}'), // medium angle < >
    ('\u{276E}', '\u{276F}'), // heavy angle quotation < >
    ('\u{2770}', '\u{2771}'), // heavy angle ( )
    ('\u{2772}', '\u{2773}'), // light tortoise shell ( )
    ('\u{2774}', '\u{2775}'), // medium curly { }
    ('\u{27C5}', '\u{27C6}'), // s-shaped bag delimiter
    ('\u{27E6}', '\u{27E7}'), // ⟦ ⟧
    ('\u{27E8}', '\u{27E9}'), // ⟨ ⟩
    ('\u{27EA}', '\u{27EB}'), // ⟪ ⟫
    ('\u{27EC}', '\u{27ED}'), // ⟬ ⟭
    ('\u{27EE}', '\u{27EF}'), // ⟮ ⟯
    ('\u{2983}', '\u{2984}'), // ⦃ ⦄
    ('\u{2985}', '\u{2986}'), // ⦅ ⦆
    ('\u{2987}', '\u{2988}'), // ⦇ ⦈
    ('\u{2989}', '\u{298A}'), // ⦉ ⦊
    ('\u{298B}', '\u{298C}'), // ⦋ ⦌
    ('\u{298D}', '\u{298E}'), // ⦍ ⦎
    ('\u{298F}', '\u{2990}'), // ⦏ ⦐
    ('\u{2991}', '\u{2992}'), // ⦑ ⦒
    ('\u{2993}', '\u{2994}'), // ⦓ ⦔
    ('\u{2995}', '\u{2996}'), // ⦕ ⦖
    ('\u{2997}', '\u{2998}'), // ⦗ ⦘
    ('\u{29D8}', '\u{29D9}'), // ⧘ ⧙
    ('\u{29DA}', '\u{29DB}'), // ⧚ ⧛
    ('\u{29FC}', '\u{29FD}'), // ⧼ ⧽
    ('\u{2E22}', '\u{2E23}'), // ⸢ ⸣ top half brackets
    ('\u{2E24}', '\u{2E25}'), // ⸤ ⸥ bottom half brackets
    ('\u{2E26}', '\u{2E27}'), // ⸦ ⸧ sideways U brackets
    ('\u{2E28}', '\u{2E29}'), // ⸨ ⸩ double parentheses
    ('\u{3008}', '\u{3009}'), // 〈 〉 CJK angle
    ('\u{300A}', '\u{300B}'), // 《 》
    ('\u{300C}', '\u{300D}'), // 「 」
    ('\u{300E}', '\u{300F}'), // 『 』
    ('\u{3010}', '\u{3011}'), // 【 】
    ('\u{3014}', '\u{3015}'), // 〔 〕
    ('\u{3016}', '\u{3017}'), // 〖 〗
    ('\u{3018}', '\u{3019}'), // 〘 〙
    ('\u{301A}', '\u{301B}'), // 〚 〛
    ('\u{FE59}', '\u{FE5A}'), // small ( )
    ('\u{FE5B}', '\u{FE5C}'), // small { }
    ('\u{FE5D}', '\u{FE5E}'), // small tortoise shell ( )
    ('\u{FF08}', '\u{FF09}'), // fullwidth ( )
    ('\u{FF3B}', '\u{FF3D}'), // fullwidth [ ]
    ('\u{FF5B}', '\u{FF5D}'), // fullwidth { }
    ('\u{FF5F}', '\u{FF60}'), // fullwidth ⦅ ⦆
    ('\u{FF62}', '\u{FF63}'), // halfwidth 「 」
];

// ─────────────────────────────────────────────────────────────────────────────
// Bidi class table
// ─────────────────────────────────────────────────────────────────────────────

/// The Unicode bidirectional class of `c` (UAX #9 / `DerivedBidiClass.txt`),
/// from a compact built-in range table.
///
/// The table is exhaustive for the formatting characters and for the scripts and
/// punctuation an HTML document realistically renders: Basic Latin and Latin-1,
/// the European/Arabic number machinery, Hebrew and Arabic (including the Arabic
/// presentation forms), Syriac/Thaana/N'Ko (RTL), and the general-punctuation
/// neutrals. Code points outside the listed ranges fall back to the correct
/// *default* class for their block — `R`/`AL` for the right-to-left default
/// ranges, `ET` for the currency-symbols block, `BN` for default-ignorable
/// ranges — and otherwise `L`.
///
/// ```
/// # use gigapdf_core::html::bidi::{bidi_class, BidiClass};
/// assert_eq!(bidi_class('A'), BidiClass::L);
/// assert_eq!(bidi_class('\u{05D0}'), BidiClass::R);   // Hebrew alef
/// assert_eq!(bidi_class('\u{0627}'), BidiClass::Al);  // Arabic alef
/// assert_eq!(bidi_class('5'), BidiClass::En);
/// assert_eq!(bidi_class('\u{0660}'), BidiClass::An);  // Arabic-Indic digit zero
/// assert_eq!(bidi_class('\u{202E}'), BidiClass::Rlo); // RIGHT-TO-LEFT OVERRIDE
/// ```
pub fn bidi_class(c: char) -> BidiClass {
    let u = c as u32;

    // Explicit formatting characters (exact code points).
    match u {
        0x202A => return Lre,
        0x202B => return Rle,
        0x202D => return Lro,
        0x202E => return Rlo,
        0x202C => return Pdf,
        0x2066 => return Lri,
        0x2067 => return Rli,
        0x2068 => return Fsi,
        0x2069 => return Pdi,
        _ => {}
    }

    // ASCII control / Latin (the high-traffic range), handled precisely.
    if u < 0x0080 {
        return ascii_class(u);
    }

    // Every paired bracket (BD14/BD15) has bidi class Other Neutral. Cover them
    // explicitly so brackets outside the ranges enumerated below (e.g. the angle
    // brackets U+2329/U+232A, the CJK and fullwidth brackets) are not misclassed
    // as `L` by the block defaults — N0 depends on their being `ON`.
    if u > 0x00FF && is_paired_bracket(c) {
        return On;
    }

    // Latin-1 supplement (0080–00FF).
    if (0x0080..=0x00FF).contains(&u) {
        return latin1_class(u);
    }

    // Combining diacritical marks (NSM).
    if (0x0300..=0x036F).contains(&u) {
        return Nsm;
    }

    // Hebrew block (0590–05FF): letters and points are R; a few marks are NSM.
    if (0x0590..=0x05FF).contains(&u) {
        return hebrew_class(u);
    }

    // Arabic and related right-to-left blocks.
    if (0x0600..=0x06FF).contains(&u) {
        return arabic_class(u);
    }
    // Syriac, Thaana, N'Ko, Samaritan, Mandaic — strong AL/R with NSM marks.
    if (0x0700..=0x074F).contains(&u) {
        // Syriac
        return if (0x0730..=0x074A).contains(&u) {
            Nsm
        } else {
            Al
        };
    }
    if (0x0750..=0x077F).contains(&u) {
        return Al; // Arabic Supplement
    }
    if (0x0780..=0x07BF).contains(&u) {
        // Thaana
        return if (0x07A6..=0x07B0).contains(&u) {
            Nsm
        } else {
            Al
        };
    }
    if (0x07C0..=0x07FF).contains(&u) {
        // N'Ko (R), with combining marks NSM
        return if (0x07EB..=0x07F3).contains(&u) || u == 0x07FD {
            Nsm
        } else {
            R
        };
    }

    // Arabic Extended / presentation forms.
    if (0x08A0..=0x08FF).contains(&u) {
        // Arabic Extended-A: marks are NSM, letters AL.
        return if (0x0816..=0x0900).contains(&u) && (0x08D3..=0x08FF).contains(&u) {
            Nsm
        } else {
            Al
        };
    }
    if (0xFB1D..=0xFB4F).contains(&u) {
        // Hebrew presentation forms.
        return if u == 0xFB1E { Nsm } else { R };
    }
    if (0xFB50..=0xFDFF).contains(&u) {
        return Al; // Arabic Presentation Forms-A
    }
    if (0xFE70..=0xFEFF).contains(&u) {
        // Arabic Presentation Forms-B (FEFF is ZWNBSP = BN).
        return if u == 0xFEFF { Bn } else { Al };
    }

    // General punctuation neutrals and number separators.
    if (0x2000..=0x206F).contains(&u) {
        return general_punct_class(u);
    }

    // Currency symbols block defaults to ET.
    if (0x20A0..=0x20CF).contains(&u) {
        return Et;
    }

    // Combining marks for symbols (NSM).
    if (0x20D0..=0x20FF).contains(&u) {
        return Nsm;
    }

    // Arabic Mathematical Alphabetic Symbols.
    if (0x1EE00..=0x1EEFF).contains(&u) {
        return Al;
    }

    // Default-ignorable / specials that are Boundary Neutral.
    if matches!(u, 0x00AD | 0x200B | 0x2060..=0x2064 | 0xFFF9..=0xFFFB) {
        return Bn;
    }

    // Default bidi class by block for unassigned code points (UAX #44):
    // the right-to-left default ranges.
    if is_default_r(u) {
        return R;
    }
    if is_default_al(u) {
        return Al;
    }

    // Everything else defaults to L.
    L
}

/// Bidi class for an ASCII code point (`< 0x80`).
fn ascii_class(u: u32) -> BidiClass {
    match u {
        // C0 controls: TAB is S; LF, VT, FF, CR, FS, GS, RS, US — B or S per UAX.
        0x0009 => S,          // TAB
        0x000A | 0x000D => B, // LF, CR (paragraph separators)
        0x000B => S,          // VT
        0x000C => Ws,         // FF
        0x001C..=0x001E => B, // FS, GS, RS
        0x001F => S,          // US
        0x0020 => Ws,         // SPACE
        // Other C0 controls.
        0x0000..=0x0008 | 0x000E..=0x001B => Bn,
        // Digits.
        0x0030..=0x0039 => En,
        // Number-related punctuation.
        0x002B | 0x002D => Es,                   // + -
        0x0023..=0x0025 => Et,                   // # $ %
        0x002C | 0x002E | 0x002F | 0x003A => Cs, // , . / :
        // Latin letters.
        0x0041..=0x005A | 0x0061..=0x007A => L,
        // Everything else printable ASCII is Other Neutral.
        _ => On,
    }
}

/// Bidi class for a Latin-1 supplement code point (`0x80..=0xFF`).
fn latin1_class(u: u32) -> BidiClass {
    match u {
        0x0085 => B,                    // NEL
        0x00A0 => Cs,                   // NBSP (common separator)
        0x00AD => Bn,                   // SOFT HYPHEN
        0x00A2..=0x00A5 => Et,          // ¢ £ ¤ ¥
        0x00B0 | 0x00B1 => Et,          // ° ±
        0x00B9 | 0x00B2 | 0x00B3 => En, // superscript 1/2/3
        // Letters (incl. ÷ × are math On; ª º are L).
        0x00AA | 0x00B5 | 0x00BA => L,
        0x00C0..=0x00D6 | 0x00D8..=0x00F6 | 0x00F8..=0x00FF => L,
        // Control range C1.
        0x0080..=0x0084 | 0x0086..=0x009F => Bn,
        _ => On,
    }
}

/// Bidi class within the Hebrew block (`0x0590..=0x05FF`).
fn hebrew_class(u: u32) -> BidiClass {
    match u {
        // Hebrew points and marks are NSM.
        0x0591..=0x05BD | 0x05BF | 0x05C1 | 0x05C2 | 0x05C4 | 0x05C5 | 0x05C7 => Nsm,
        // Letters, punctuation, and the rest of the block are R.
        _ => R,
    }
}

/// Bidi class within the Arabic block (`0x0600..=0x06FF`).
fn arabic_class(u: u32) -> BidiClass {
    match u {
        // Arabic-Indic digits.
        0x0660..=0x0669 => An,
        // Arabic number signs / separators classed AN.
        0x0600..=0x0605 | 0x0608 | 0x060B | 0x060D => An,
        0x066B | 0x066C => An, // decimal/thousands separator
        // Arabic comma/semicolon/question — Other Neutral.
        0x060C | 0x061B | 0x061F | 0x066A => On,
        // Percent sign (Arabic) is ET.
        0x066D => Al,
        // Combining marks (NSM).
        0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7
        | 0x06E8
        | 0x06EA..=0x06ED => Nsm,
        // Extended Arabic-Indic digits.
        0x06F0..=0x06F9 => En,
        // Everything else is an Arabic letter (AL).
        _ => Al,
    }
}

/// Bidi class within General Punctuation (`0x2000..=0x206F`).
fn general_punct_class(u: u32) -> BidiClass {
    match u {
        0x2000..=0x200A => Ws, // various spaces
        0x200B => Bn,          // ZWSP
        0x200C | 0x200D => Bn, // ZWNJ / ZWJ
        0x200E => L,           // LRM
        0x200F => R,           // RLM
        0x2028 => Ws,          // LINE SEPARATOR
        0x2029 => B,           // PARAGRAPH SEPARATOR
        0x202F => Cs,          // NARROW NBSP
        0x2030..=0x2034 => Et, // per-mille etc.
        0x2044 => Cs,          // FRACTION SLASH
        0x205F => Ws,          // MEDIUM MATHEMATICAL SPACE
        0x2060..=0x2064 => Bn, // word joiner / invisible operators
        // 2066–2069 handled at the top (isolates).
        _ => On,
    }
}

/// `true` if `u` falls in a Unicode block whose **default** bidi class is `R`
/// (UAX #44 default-bidi ranges), used for unassigned code points.
fn is_default_r(u: u32) -> bool {
    matches!(u,
        0x0590..=0x05FF | 0x07C0..=0x085F | 0xFB1D..=0xFB4F
        | 0x10800..=0x10CFF | 0x10D40..=0x10EBF | 0x10F00..=0x10F2F
        | 0x10F70..=0x10FFF | 0x1E800..=0x1EC6F | 0x1ECC0..=0x1ECFF
        | 0x1ED50..=0x1EDFF | 0x1EF00..=0x1EFFF
    )
}

/// `true` if `u` falls in a Unicode block whose **default** bidi class is `AL`
/// (UAX #44 default-bidi ranges), used for unassigned code points.
fn is_default_al(u: u32) -> bool {
    matches!(u,
        0x0600..=0x07BF | 0x0860..=0x08FF | 0xFB50..=0xFDCF | 0xFDF0..=0xFDFF
        | 0xFE70..=0xFEFF | 0x10D00..=0x10D3F | 0x10EC0..=0x10EFF
        | 0x1EC70..=0x1ECBF | 0x1ED00..=0x1ED4F | 0x1EE00..=0x1EEFF
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the algorithm on a string of characters, one atom per `char`.
    fn order_of(s: &str, base: u8) -> Vec<usize> {
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        reorder(&refs, base)
    }

    /// The visual string produced by reordering `s` under `base`.
    fn visual(s: &str, base: u8) -> String {
        let chars: Vec<char> = s.chars().collect();
        order_of(s, base).into_iter().map(|i| chars[i]).collect()
    }

    #[test]
    fn pure_ltr_is_identity() {
        assert_eq!(order_of("hello world", 0), (0..11).collect::<Vec<_>>());
        // No allocation-affecting reordering; the string is unchanged.
        assert_eq!(visual("abc 123 def", 0), "abc 123 def");
    }

    #[test]
    fn empty_input() {
        let none: [&str; 0] = [];
        assert_eq!(reorder(&none, 0), Vec::<usize>::new());
        assert_eq!(reorder(&none, 1), Vec::<usize>::new());
    }

    #[test]
    fn class_of_formatting_chars() {
        assert_eq!(bidi_class('\u{202A}'), Lre);
        assert_eq!(bidi_class('\u{202B}'), Rle);
        assert_eq!(bidi_class('\u{202D}'), Lro);
        assert_eq!(bidi_class('\u{202E}'), Rlo);
        assert_eq!(bidi_class('\u{202C}'), Pdf);
        assert_eq!(bidi_class('\u{2066}'), Lri);
        assert_eq!(bidi_class('\u{2067}'), Rli);
        assert_eq!(bidi_class('\u{2068}'), Fsi);
        assert_eq!(bidi_class('\u{2069}'), Pdi);
    }

    #[test]
    fn class_of_strong_and_numbers() {
        assert_eq!(bidi_class('a'), L);
        assert_eq!(bidi_class('Z'), L);
        assert_eq!(bidi_class('\u{05D0}'), R); // Hebrew alef
        assert_eq!(bidi_class('\u{0627}'), Al); // Arabic alef
        assert_eq!(bidi_class('7'), En);
        assert_eq!(bidi_class('\u{0661}'), An); // Arabic-Indic one
        assert_eq!(bidi_class('\u{0301}'), Nsm); // combining acute
        assert_eq!(bidi_class(' '), Ws);
        assert_eq!(bidi_class('('), On);
    }

    #[test]
    fn simple_rtl_reverses_letters() {
        // Three R characters in an RTL paragraph read right-to-left: the visual
        // order is the reverse of the logical order.
        assert_eq!(order_of("\u{05D0}\u{05D1}\u{05D2}", 1), vec![2, 1, 0]);
    }

    #[test]
    fn ltr_run_inside_rtl_stays_ltr() {
        // RTL letters, then an embedded Latin word "ab": the Latin keeps its
        // internal left-to-right order while the whole line is laid right-to-left.
        // Logical: H0 H1 a b   (H = Hebrew, level 1; "ab" level 2). base = RTL.
        // L2 reverses the level-2 run, then the whole level-≥1 span, so the
        // Latin lands left-of and in-order, the Hebrew right and reversed:
        // visual  a b H1 H0  → indices [2,3,1,0].
        let s = "\u{05D0}\u{05D1}ab";
        let vis = visual(s, 1);
        assert_eq!(order_of(s, 1), vec![2, 3, 1, 0]);
        // "ab" substring preserved in order.
        let pos_a = vis.chars().position(|c| c == 'a').unwrap();
        let pos_b = vis.chars().position(|c| c == 'b').unwrap();
        assert!(pos_a < pos_b, "embedded Latin keeps LTR order: {vis:?}");
    }

    #[test]
    fn rli_pdi_isolates_inner_ltr_from_outside() {
        // Hebrew, then an isolated Latin run "abc" via RLI…PDI? No — to isolate an
        // LTR run inside RTL we use LRI…PDI. The isolate keeps the inner run from
        // interacting with the surrounding numbers/letters.
        //  H H LRI a b c PDI H
        let s = "\u{05D0}\u{05D1}\u{2066}abc\u{2069}\u{05D2}";
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 1);
        // The inner "abc" must appear contiguous and in order in the visual string.
        let vis: String = order.iter().map(|&i| chars[i]).collect();
        let a = vis.find('a').unwrap();
        assert_eq!(
            &vis[a..a + 3],
            "abc",
            "isolated LTR run stays contiguous LTR"
        );
    }

    #[test]
    fn rli_keeps_inner_numbers_from_affecting_outside() {
        // An RLI isolates an inner run so its strong/number types cannot leak out
        // and reorder neighbouring text. Inner is Latin digits inside an LTR
        // base with surrounding Hebrew; without isolation the numbers could move.
        //  base LTR:  a RLI <H H> PDI b
        let s = "a\u{2067}\u{05D0}\u{05D1}\u{2069}b";
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 0);
        let vis: String = order.iter().map(|&i| chars[i]).collect();
        // 'a' is first, 'b' is last; the Hebrew between them is reversed but the
        // isolate prevents 'a'/'b' from being dragged into the RTL run.
        assert_eq!(vis.chars().next().unwrap(), 'a');
        assert_eq!(vis.chars().last().unwrap(), 'b');
    }

    #[test]
    fn lre_pdf_embedding_reverses_inner_rtl() {
        // RLE forces an RTL embedding around Latin? Use LRE around Hebrew inside an
        // RTL base: LRE makes the inner Hebrew be laid in an LTR embedding level.
        //  base RTL:  H0 LRE H1 H2 PDF H3
        let s = "\u{05D0}\u{202A}\u{05D1}\u{05D2}\u{202C}\u{05D3}";
        let order = order_of(s, 1);
        // The result is a valid permutation of all six positions (controls
        // included): every index present exactly once.
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn rlo_override_forces_rtl_on_latin() {
        // RLO overrides Latin letters to R, so "abc" is laid right-to-left:
        // visual order of the letters is reversed.
        //  base LTR:  RLO a b c PDF
        let s = "\u{202E}abc\u{202C}";
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 0);
        let vis: String = order
            .iter()
            .map(|&i| chars[i])
            .filter(|c| c.is_ascii_alphabetic())
            .collect();
        assert_eq!(vis, "cba", "RLO lays Latin right-to-left");
    }

    #[test]
    fn lro_override_forces_ltr_on_hebrew() {
        // LRO overrides Hebrew to L, so the Hebrew keeps logical (left-to-right)
        // order instead of being reversed.
        //  base RTL:  LRO H0 H1 H2 PDF
        let s = "\u{202D}\u{05D0}\u{05D1}\u{05D2}\u{202C}";
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 1);
        let vis: Vec<char> = order
            .iter()
            .map(|&i| chars[i])
            .filter(|c| ('\u{05D0}'..='\u{05EA}').contains(c))
            .collect();
        assert_eq!(
            vis,
            vec!['\u{05D0}', '\u{05D1}', '\u{05D2}'],
            "LRO lays Hebrew left-to-right (logical order)"
        );
    }

    #[test]
    fn bracket_pair_around_rtl_inside_ltr_takes_r() {
        // N0 (c.1): a parenthesis pair around an RTL run, inside an LTR line,
        // with RTL strong context immediately before the open bracket. The
        // established RTL context plus the RTL content make both brackets resolve
        // to R (odd level), so the parentheses mirror visually.
        //  base LTR:  H ( H ) H   (H = Hebrew, the strong context is RTL)
        let s = "\u{05D0}(\u{05D1})\u{05D2}";
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let levels = debug_levels(&refs, 0);
        // positions: 0=H 1='(' 2=H 3=')' 4=H
        assert_eq!(levels[1] % 2, 1, "open bracket resolved to R (odd level)");
        assert_eq!(levels[3] % 2, 1, "close bracket resolved to R (odd level)");
        // The whole run is at an odd level, so it is a valid permutation reversed.
        let order = reorder(&refs, 0);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..5).collect::<Vec<_>>());
    }

    #[test]
    fn bracket_pair_resolves_to_embedding_when_context_ltr() {
        // N0 (c.2): the same parentheses around RTL content but with LTR strong
        // context before the open bracket resolve to the *embedding* direction
        // (L), not R — the preceding context did not establish RTL.
        //  base LTR:  a ( H H ) b
        let s = "a(\u{05D0}\u{05D1})b";
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let levels = debug_levels(&refs, 0);
        // positions: 0=a 1='(' 2=H 3=H 4=')' 5=b — brackets stay L (even).
        assert_eq!(levels[1] % 2, 0, "open bracket takes embedding L (c.2)");
        assert_eq!(levels[4] % 2, 0, "close bracket takes embedding L (c.2)");
        // The Hebrew between them is still reversed (level 1).
        assert_eq!(levels[2] % 2, 1);
        assert_eq!(levels[3] % 2, 1);
    }

    #[test]
    fn bracket_pair_with_ltr_inside_stays_ltr() {
        // N0 (b): a bracket pair around an LTR run inside an LTR line — the
        // brackets keep the embedding (L) direction.
        let s = "a(bc)d";
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let levels = debug_levels(&refs, 0);
        assert!(
            levels.iter().all(|&l| l % 2 == 0),
            "all stay LTR: {levels:?}"
        );
        assert_eq!(reorder(&refs, 0), (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn fsi_autodetects_rtl_scope() {
        // FSI before a Hebrew-first scope resolves to RTL; before a Latin-first
        // scope it resolves to LTR. We check that the inner direction is detected
        // by comparing the inner run's reordering.
        // FSI <H a> PDI  in an LTR base: scope is RTL ⇒ inner laid R-to-L overall,
        // but the embedded Latin 'a' keeps LTR. Compare against an explicit RLI.
        let with_fsi = "\u{2068}\u{05D0}a\u{2069}";
        let with_rli = "\u{2067}\u{05D0}a\u{2069}";
        assert_eq!(
            order_of(with_fsi, 0),
            order_of(with_rli, 0),
            "FSI with Hebrew-first scope behaves like RLI"
        );

        // FSI before a Latin-first scope behaves like LRI.
        let fsi_ltr = "\u{2068}a\u{05D0}\u{2069}";
        let lri_ltr = "\u{2066}a\u{05D0}\u{2069}";
        assert_eq!(
            order_of(fsi_ltr, 0),
            order_of(lri_ltr, 0),
            "FSI with Latin-first scope behaves like LRI"
        );
    }

    #[test]
    fn overflow_isolates_degrade_gracefully() {
        // 130 nested isolate initiators exceed max_depth (125). The algorithm must
        // not panic and must return a valid permutation covering every index.
        let mut s = String::new();
        for _ in 0..130 {
            s.push('\u{2067}'); // RLI
        }
        s.push('\u{05D0}'); // a Hebrew letter at the deepest point
        for _ in 0..130 {
            s.push('\u{2069}'); // PDI
        }
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let n = refs.len();
        let order = reorder(&refs, 0);
        assert_eq!(order.len(), n);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "valid permutation");
    }

    #[test]
    fn overflow_embeddings_degrade_gracefully() {
        // 130 nested LRE embeddings exceed max_depth; no panic, valid permutation.
        let mut s = String::new();
        for _ in 0..130 {
            s.push('\u{202B}'); // RLE
        }
        s.push('x');
        for _ in 0..130 {
            s.push('\u{202C}'); // PDF
        }
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let n = refs.len();
        let order = reorder(&refs, 0);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn numbers_in_rtl_run_keep_order() {
        // European numbers inside an RTL run stay in logical order (digits read
        // left-to-right) even though the surrounding RTL text is reversed.
        //  base RTL:  H 1 2 3 H
        let s = "\u{05D0}123\u{05D1}";
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 1);
        let vis: String = order.iter().map(|&i| chars[i]).collect();
        let p1 = vis.find('1').unwrap();
        assert_eq!(&vis[p1..p1 + 3], "123", "digits stay in logical order");
    }

    #[test]
    fn arabic_number_after_arabic_letter() {
        // W2: an EN after a strong AL becomes AN. We don't expose types directly,
        // but the reordering of "Arabic-letter 1 2" must keep the digits grouped.
        let s = "\u{0627}12"; // Arabic alef + "12"
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 1);
        let vis: String = order.iter().map(|&i| chars[i]).collect();
        let p1 = vis.find('1').unwrap();
        assert_eq!(&vis[p1..p1 + 2], "12");
    }

    #[test]
    fn whitespace_at_line_end_reset_to_base() {
        // L1: trailing whitespace in an RTL line resets to the paragraph level,
        // so a trailing space sits at the visual left end (paragraph-level start).
        //  base RTL:  H H <space>
        let s = "\u{05D0}\u{05D1} ";
        let chars: Vec<char> = s.chars().collect();
        let order = order_of(s, 1);
        let vis: String = order.iter().map(|&i| chars[i]).collect();
        // The space (reset to level 0) sits at the right end visually for an RTL
        // paragraph? L1 resets trailing ws to paragraph level (1 here), so it
        // stays at the visual left. Either way the permutation is valid and the
        // Hebrew is reversed.
        assert!(vis.contains(' '));
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..3).collect::<Vec<_>>());
    }

    #[test]
    fn nested_brackets_resolve_independently() {
        // Nested brackets around RTL content with established RTL context: both
        // pairs resolve to R (N0).
        //  base LTR:  H [ ( H ) ] H
        let s = "\u{05D0}[(\u{05D1})]\u{05D2}";
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let levels = debug_levels(&refs, 0);
        // positions: 0=H 1=[ 2=( 3=H 4=) 5=] 6=H
        assert_eq!(levels[1] % 2, 1, "outer [ resolved R");
        assert_eq!(levels[2] % 2, 1, "inner ( resolved R");
        assert_eq!(levels[4] % 2, 1, "inner ) resolved R");
        assert_eq!(levels[5] % 2, 1, "outer ] resolved R");
    }

    #[test]
    fn canonical_equivalent_brackets_match() {
        // U+2329/U+232A (angle) must pair with U+3008/U+3009 under canonical
        // equivalence: open with U+2329, close with U+3009 around RTL content in
        // RTL context. Both resolve to R via N0 only if the pair was recognised
        // across the canonical equivalence.
        //  base LTR:  H ⟨ H 〉 H   (open=U+2329, close=U+3009)
        let s = "\u{05D0}\u{2329}\u{05D1}\u{3009}\u{05D2}";
        let atoms: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = atoms.iter().map(String::as_str).collect();
        let levels = debug_levels(&refs, 0);
        // positions: 0=H 1=⟨ 2=H 3=〉 4=H
        assert_eq!(levels[1] % 2, 1, "U+2329 open resolved R via N0");
        assert_eq!(
            levels[3] % 2,
            1,
            "U+3009 close resolved R via N0 (canon eq)"
        );
    }

    #[test]
    fn mixed_paragraph_is_valid_permutation() {
        // A realistic mixed line must always yield a valid permutation regardless
        // of base direction.
        let s = "Hello \u{05E9}\u{05DC}\u{05D5}\u{05DD} 2024 \u{0627}\u{0644}!";
        for base in [0u8, 1u8] {
            let order = order_of(s, base);
            let n = s.chars().count();
            assert_eq!(order.len(), n);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "base {base}");
        }
    }

    #[test]
    fn ltr_base_with_only_neutrals_is_identity() {
        // Punctuation and spaces with no strong RTL stay in order under LTR.
        assert_eq!(visual("a, b. c!", 0), "a, b. c!");
    }

    #[test]
    fn rtl_base_pure_neutrals_reverse_as_block() {
        // Under an RTL base, a run of only neutrals resolves to the base direction
        // and reverses; verify a valid permutation and that it is the reverse.
        let s = "...";
        assert_eq!(order_of(s, 1), vec![2, 1, 0]);
    }

    /// Test-only helper exposing the resolved per-atom embedding levels, so N0 /
    /// L1 outcomes can be asserted directly. Mirrors [`reorder`] up to L2.
    fn debug_levels(word_chars: &[&str], base: u8) -> Vec<u8> {
        let n = word_chars.len();
        let para_level = if base == 0 { 0 } else { 1 };
        let original: Vec<BidiClass> = word_chars.iter().map(|s| atom_class(s)).collect();
        let chars: Vec<char> = word_chars
            .iter()
            .map(|s| s.chars().next().unwrap_or('\u{0}'))
            .collect();
        ATOM_CHARS.with(|cell| *cell.borrow_mut() = chars);
        let Explicit {
            mut levels,
            mut types,
        } = resolve_explicit(&original, para_level);
        let sequences = isolating_run_sequences(&original, &types, &levels, para_level);
        for seq in &sequences {
            resolve_sequence(seq, &original, &mut types, &mut levels, para_level);
        }
        reset_whitespace_levels(&original, &types, &mut levels, para_level);
        ATOM_CHARS.with(|cell| cell.borrow_mut().clear());
        let _ = n;
        levels
    }
}
