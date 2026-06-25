//! JPEG 2000 tier-1 EBCOT bit-plane decoding (ISO/IEC 15444-1 Annex D).
//!
//! Decodes one code-block's coded byte segment into signed wavelet coefficients
//! by running the three coding passes (significance-propagation,
//! magnitude-refinement, cleanup) over each bit-plane from the most-significant
//! non-zero plane down, driven by the shared MQ arithmetic coder
//! ([`crate::filters::jbig2_mq`]) with the 19 JPEG 2000 context labels.
//!
//! Output: a `width*height` buffer of magnitudes (the significant bits decoded
//! so far, LSB-aligned to the lowest decoded bit-plane) and a parallel sign
//! buffer, plus the number of low bit-planes left undecoded (`lsb_shift`) so the
//! dequantiser ([`super::dwt`]) can place the magnitude at its true bit weight.

use crate::filters::jbig2_mq::{ArithContext, MqDecoder};

// Context label indices (the 19 JPEG 2000 contexts, ISO/IEC 15444-1 Table D.1).
// 0..=8  : zero-coding (significance) contexts.
// 9..=13 : sign-coding contexts.
// 14..=16: magnitude-refinement contexts.
// 17     : run-length context.
// 18     : UNIFORM context.
const NUM_CTX: usize = 19;
const CTX_RL: usize = 17;
const CTX_UNI: usize = 18;
const CTX_SIGN_BASE: usize = 9;
const CTX_MR_BASE: usize = 14;

// Per-coefficient state bit-flags (kept in a u8 grid alongside magnitudes).
const SIG: u8 = 1 << 0; // coefficient is significant
const VISIT: u8 = 1 << 1; // visited in the significance-propagation pass
const REFINED: u8 = 1 << 2; // has had at least one magnitude-refinement bit

/// The result of decoding one code-block.
#[derive(Debug)]
pub struct CodeBlockResult {
    /// Magnitudes (non-negative), LSB-aligned to the lowest decoded plane.
    pub mag: Vec<u32>,
    /// Sign per coefficient (`true` = negative).
    pub sign: Vec<bool>,
    /// Number of undecoded low bit-planes (true bit weight of the LSB).
    pub lsb_shift: u32,
}

/// Decode a code-block's coded segment.
///
/// * `data` — the concatenated coded bytes for the block.
/// * `w`,`h` — code-block dimensions.
/// * `mb` — total magnitude bit-planes for the subband (`G + exponent - 1`).
/// * `zero_bit_planes` — leading all-zero planes (from the tag-tree).
/// * `num_passes` — total coding passes signalled across all layers.
/// * `orient` — subband orientation kind (0 = LL/LH, 1 = HL, 2 = HH) for ZC.
/// * `cb_style` — code-block style flags (`SPcod`/`SPcoc` cbstyle byte).
#[allow(clippy::too_many_arguments)]
pub fn decode_codeblock(
    data: &[u8],
    w: usize,
    h: usize,
    mb: u32,
    zero_bit_planes: u32,
    num_passes: u32,
    orient: u8,
    cb_style: u8,
) -> CodeBlockResult {
    let mut t1 = T1::new(w, h, orient, cb_style);
    if w == 0 || h == 0 || num_passes == 0 || data.is_empty() {
        return t1.into_result(0);
    }
    // Most-significant decoded plane index (0-based from bit 0). The first coding
    // pass is a cleanup of plane `p0`.
    let p0 = mb.saturating_sub(1).saturating_sub(zero_bit_planes);
    let mut mq = MqDecoder::new(data);

    // Pass schedule: pass 0 = cleanup(p0); then for each lower plane, the triple
    // (significance-propagation, magnitude-refinement, cleanup).
    let mut plane = p0 as i64;
    let mut pass = 0u32;
    let mut lowest_plane = p0 as i64;
    while pass < num_passes && plane >= 0 {
        if pass == 0 {
            t1.cleanup_pass(&mut mq, plane as u32);
            lowest_plane = plane;
            pass += 1;
        } else {
            // Significance propagation.
            if pass < num_passes {
                t1.sig_prop_pass(&mut mq, plane as u32);
                lowest_plane = plane;
                pass += 1;
            }
            // Magnitude refinement.
            if pass < num_passes {
                t1.mag_ref_pass(&mut mq, plane as u32);
                pass += 1;
            }
            // Cleanup.
            if pass < num_passes {
                t1.cleanup_pass(&mut mq, plane as u32);
                pass += 1;
            }
        }
        if pass == 1 {
            // After the very first (cleanup) pass move to the next lower plane.
            plane -= 1;
        } else if (pass - 1).is_multiple_of(3) {
            // Completed a full triple for `plane`; descend.
            plane -= 1;
        }
    }
    let lsb_shift = lowest_plane.max(0) as u32;
    t1.into_result(lsb_shift)
}

/// Tier-1 working state for one code-block.
struct T1 {
    w: usize,
    h: usize,
    orient: u8,
    cb_style: u8,
    /// Per-coefficient flags (`SIG`/`VISIT`/`REFINED`).
    flags: Vec<u8>,
    /// Per-coefficient magnitude accumulator (bits set as planes are decoded).
    mag: Vec<u32>,
    /// Per-coefficient sign (`true` = negative).
    sign: Vec<bool>,
    ctx: [ArithContext; NUM_CTX],
}

impl T1 {
    fn new(w: usize, h: usize, orient: u8, cb_style: u8) -> Self {
        let mut ctx = [ArithContext::default(); NUM_CTX];
        // Initial states (ISO/IEC 15444-1 §D.3.2 / Annex C): UNIFORM → index 46,
        // run-length → index 3, the all-zero ZC context (index 0) → index 4.
        ctx[CTX_UNI] = ArithContext::with(46, 0);
        ctx[CTX_RL] = ArithContext::with(3, 0);
        ctx[0] = ArithContext::with(4, 0);
        T1 {
            w,
            h,
            orient,
            cb_style,
            flags: vec![0u8; w * h],
            mag: vec![0u32; w * h],
            sign: vec![false; w * h],
            ctx,
        }
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.w + x
    }

    fn is_sig(&self, x: i64, y: i64) -> bool {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return false;
        }
        self.flags[(y as usize) * self.w + x as usize] & SIG != 0
    }

    fn sign_of(&self, x: i64, y: i64) -> i32 {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return 0;
        }
        let i = (y as usize) * self.w + x as usize;
        if self.flags[i] & SIG == 0 {
            0
        } else if self.sign[i] {
            -1
        } else {
            1
        }
    }

    /// Significance-propagation pass over plane `p` (Annex D.3.1).
    fn sig_prop_pass(&mut self, mq: &mut MqDecoder, p: u32) {
        let bit = 1u32 << p;
        for y0 in (0..self.h).step_by(4) {
            for x in 0..self.w {
                for y in y0..(y0 + 4).min(self.h) {
                    let i = self.idx(x, y);
                    self.flags[i] &= !VISIT;
                    if self.flags[i] & SIG != 0 {
                        continue;
                    }
                    if self.zc_context_nonzero(x, y) == 0 {
                        continue; // no significant neighbour → skip in this pass
                    }
                    let cx = self.zc_context(x, y);
                    if mq.decode(&mut self.ctx[cx]) == 1 {
                        self.decode_sign(mq, x, y);
                        let i = self.idx(x, y);
                        self.flags[i] |= SIG;
                        self.mag[i] |= bit;
                    }
                    let i = self.idx(x, y);
                    self.flags[i] |= VISIT;
                }
            }
        }
    }

    /// Magnitude-refinement pass over plane `p` (Annex D.3.3).
    fn mag_ref_pass(&mut self, mq: &mut MqDecoder, p: u32) {
        let bit = 1u32 << p;
        for y0 in (0..self.h).step_by(4) {
            for x in 0..self.w {
                for y in y0..(y0 + 4).min(self.h) {
                    let i = self.idx(x, y);
                    if self.flags[i] & SIG == 0 {
                        continue;
                    }
                    if self.flags[i] & VISIT != 0 {
                        continue; // became significant in this plane's SP pass
                    }
                    let cx = self.mr_context(x, y);
                    let b = mq.decode(&mut self.ctx[cx]);
                    let i = self.idx(x, y);
                    if b == 1 {
                        self.mag[i] |= bit;
                    }
                    self.flags[i] |= REFINED;
                }
            }
        }
    }

    /// Cleanup pass over plane `p` (Annex D.3.4), with the run-length mode.
    fn cleanup_pass(&mut self, mq: &mut MqDecoder, p: u32) {
        let bit = 1u32 << p;
        for y0 in (0..self.h).step_by(4) {
            for x in 0..self.w {
                let col_h = (y0 + 4).min(self.h) - y0;
                let mut y = y0;
                // Run-length: if the whole 4-high column strip is insignificant
                // and has no significant neighbour, code a single run bit.
                if col_h == 4 && self.strip_all_insig_no_neighbour(x, y0) {
                    if mq.decode(&mut self.ctx[CTX_RL]) == 0 {
                        // Entire strip stays insignificant; clear visit flags.
                        for yy in y0..y0 + 4 {
                            let i = self.idx(x, yy);
                            self.flags[i] &= !VISIT;
                        }
                        continue;
                    }
                    // First significant coefficient: 2-bit UNIFORM run index.
                    let hi = mq.decode(&mut self.ctx[CTX_UNI]);
                    let lo = mq.decode(&mut self.ctx[CTX_UNI]);
                    let run = ((hi << 1) | lo) as usize;
                    y = y0 + run;
                    self.decode_sign(mq, x, y);
                    let i = self.idx(x, y);
                    self.flags[i] |= SIG;
                    self.mag[i] |= bit;
                    self.flags[i] &= !VISIT;
                    y += 1;
                }
                while y < (y0 + 4).min(self.h) {
                    let i = self.idx(x, y);
                    if self.flags[i] & (SIG | VISIT) != 0 {
                        self.flags[i] &= !VISIT;
                        y += 1;
                        continue;
                    }
                    let cx = self.zc_context(x, y);
                    if mq.decode(&mut self.ctx[cx]) == 1 {
                        self.decode_sign(mq, x, y);
                        let i = self.idx(x, y);
                        self.flags[i] |= SIG;
                        self.mag[i] |= bit;
                    }
                    let i = self.idx(x, y);
                    self.flags[i] &= !VISIT;
                    y += 1;
                }
            }
        }
        // Optional segmentation symbol (cb_style bit 5 = SEGSYM): consume the
        // 4-bit UNIFORM-coded 0b1010 marker if present.
        if self.cb_style & 0x20 != 0 {
            let _ = self.decode_segsym(mq);
        }
    }

    fn decode_segsym(&mut self, mq: &mut MqDecoder) -> u32 {
        let mut v = 0;
        for _ in 0..4 {
            v = (v << 1) | mq.decode(&mut self.ctx[CTX_UNI]) as u32;
        }
        v
    }

    /// `true` when a 4-high strip column is entirely insignificant AND no
    /// coefficient in it has a significant neighbour (run-length precondition).
    fn strip_all_insig_no_neighbour(&self, x: usize, y0: usize) -> bool {
        for y in y0..y0 + 4 {
            let i = self.idx(x, y);
            if self.flags[i] & SIG != 0 {
                return false;
            }
            if self.zc_context_nonzero(x, y) != 0 {
                return false;
            }
        }
        true
    }

    /// Decode and store the sign of the coefficient at `(x, y)` (Annex D.3.2).
    fn decode_sign(&mut self, mq: &mut MqDecoder, x: usize, y: usize) {
        let (ctx, xor) = self.sign_context(x, y);
        let bit = mq.decode(&mut self.ctx[ctx]) as i32;
        let s = bit ^ xor; // 1 = negative
        let i = self.idx(x, y);
        self.sign[i] = s == 1;
    }

    /// Zero-coding context label (Annex D.3.1, Table D.1) for the orientation.
    fn zc_context(&self, x: usize, y: usize) -> usize {
        let (xi, yi) = (x as i64, y as i64);
        let h = self.is_sig(xi - 1, yi) as u32 + self.is_sig(xi + 1, yi) as u32;
        let v = self.is_sig(xi, yi - 1) as u32 + self.is_sig(xi, yi + 1) as u32;
        let d = self.is_sig(xi - 1, yi - 1) as u32
            + self.is_sig(xi + 1, yi - 1) as u32
            + self.is_sig(xi - 1, yi + 1) as u32
            + self.is_sig(xi + 1, yi + 1) as u32;
        zc_label(self.orient, h, v, d)
    }

    /// Non-zero iff `(x, y)` has at least one significant 8-neighbour.
    fn zc_context_nonzero(&self, x: usize, y: usize) -> u32 {
        let (xi, yi) = (x as i64, y as i64);
        (self.is_sig(xi - 1, yi)
            || self.is_sig(xi + 1, yi)
            || self.is_sig(xi, yi - 1)
            || self.is_sig(xi, yi + 1)
            || self.is_sig(xi - 1, yi - 1)
            || self.is_sig(xi + 1, yi - 1)
            || self.is_sig(xi - 1, yi + 1)
            || self.is_sig(xi + 1, yi + 1)) as u32
    }

    /// Sign-coding context and XOR bit (Annex D.3.2, Table D.2).
    fn sign_context(&self, x: usize, y: usize) -> (usize, i32) {
        let (xi, yi) = (x as i64, y as i64);
        let h = (self.sign_of(xi - 1, yi) + self.sign_of(xi + 1, yi)).clamp(-1, 1);
        let v = (self.sign_of(xi, yi - 1) + self.sign_of(xi, yi + 1)).clamp(-1, 1);
        // Table D.2: contribution → context offset 0..4 and the sign-flip XOR.
        let (ctx_off, xor) = match (h, v) {
            (1, 1) => (4, 0),
            (1, 0) => (3, 0),
            (1, -1) => (2, 0),
            (0, 1) => (1, 0),
            (0, 0) => (0, 0),
            (0, -1) => (1, 1),
            (-1, 1) => (2, 1),
            (-1, 0) => (3, 1),
            (-1, -1) => (4, 1),
            _ => (0, 0),
        };
        (CTX_SIGN_BASE + ctx_off, xor)
    }

    /// Magnitude-refinement context (Annex D.3.3): depends on whether this is the
    /// first refinement and the neighbourhood significance.
    fn mr_context(&self, x: usize, y: usize) -> usize {
        let i = self.idx(x, y);
        if self.flags[i] & REFINED == 0 {
            // First refinement: context 14 if no significant neighbour, else 15.
            let (xi, yi) = (x as i64, y as i64);
            let any = self.is_sig(xi - 1, yi)
                || self.is_sig(xi + 1, yi)
                || self.is_sig(xi, yi - 1)
                || self.is_sig(xi, yi + 1)
                || self.is_sig(xi - 1, yi - 1)
                || self.is_sig(xi + 1, yi - 1)
                || self.is_sig(xi - 1, yi + 1)
                || self.is_sig(xi + 1, yi + 1);
            CTX_MR_BASE + any as usize
        } else {
            CTX_MR_BASE + 2 // 16: subsequent refinements
        }
    }

    fn into_result(self, lsb_shift: u32) -> CodeBlockResult {
        CodeBlockResult {
            mag: self.mag,
            sign: self.sign,
            lsb_shift,
        }
    }
}

/// Test-only re-export of [`zc_label`] so the in-test EBCOT encoder forms the
/// exact same zero-coding contexts as this decoder.
#[cfg(test)]
pub(super) fn zc_label_for_test(orient: u8, h: u32, v: u32, d: u32) -> usize {
    zc_label(orient, h, v, d)
}

/// Map the (horizontal, vertical, diagonal) significant-neighbour counts to a
/// zero-coding context label (ISO/IEC 15444-1 Table D.1). `orient`: 0 = LL/LH,
/// 1 = HL, 2 = HH.
fn zc_label(orient: u8, h: u32, v: u32, d: u32) -> usize {
    match orient {
        2 => {
            // HH band: keyed on (d, h+v).
            let hv = h + v;
            match d {
                0 => match hv {
                    0 => 0,
                    1 => 1,
                    _ => 2,
                },
                1 => match hv {
                    0 => 3,
                    1 => 4,
                    _ => 5,
                },
                2 => match hv {
                    0 => 6,
                    _ => 7,
                },
                _ => 8,
            }
        }
        1 => {
            // HL band: rows/cols swapped relative to LH/LL — key on (v, h, d) with
            // h and v exchanged.
            zc_label_ll(v, h, d)
        }
        _ => zc_label_ll(h, v, d),
    }
}

/// Zero-coding label for the LL/LH orientation (Table D.1, columns for LL/LH).
fn zc_label_ll(h: u32, v: u32, d: u32) -> usize {
    match h {
        2 => 8,
        1 => match v {
            0 => match d {
                0 => 5,
                _ => 6,
            },
            _ => 7,
        },
        _ => match v {
            2 => 8,
            1 => 4,
            _ => match d {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => 3,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_codeblock_is_all_zero() {
        let r = decode_codeblock(&[], 4, 4, 8, 0, 0, 0, 0);
        assert_eq!(r.mag.len(), 16);
        assert!(r.mag.iter().all(|&m| m == 0));
    }

    #[test]
    fn zc_labels_ll_basic() {
        // No neighbours → context 0; two horizontal → 8.
        assert_eq!(zc_label(0, 0, 0, 0), 0);
        assert_eq!(zc_label(0, 2, 0, 0), 8);
        assert_eq!(zc_label(0, 0, 2, 0), 8);
        // HL swaps h/v: two vertical neighbours → 8.
        assert_eq!(zc_label(1, 0, 2, 0), 8);
    }

    #[test]
    fn arith_context_with_sets_fields() {
        let c = ArithContext::with(46, 1);
        assert_eq!(c.index, 46);
        assert_eq!(c.mps, 1);
    }
}
