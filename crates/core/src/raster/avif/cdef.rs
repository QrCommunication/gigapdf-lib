//! AV1 Constrained Directional Enhancement Filter (CDEF, §7.15).
//!
//! This module holds the building blocks of CDEF, transcribed bit-for-bit from
//! dav1d (`src/cdef_tmpl.c`, BSD-2-Clause) and validated against reference
//! vectors emitted by `tools/extract_av1_cdef/harness.c`:
//!   - [`constrain`] — the soft-threshold applied to every tap difference.
//!   - [`cdef_find_dir`] — the per-8×8 luma direction search (0..=7) + variance.
//!
//! The filter block itself and the apply loop are layered on top of these.

// Primitives are validated bit-exact before the apply loop wires them in
// (matches the sibling avif kernels: cdf/deblock/itx/msac/predict).
#![allow(dead_code)]

/// Soft-threshold a tap difference toward zero — dav1d `constrain`. `shift`
/// (the damping-derived right shift) bleeds the magnitude away as `diff` grows,
/// so only differences within `threshold` of the centre pixel contribute fully.
/// Sign-preserving; the result has the same sign as `diff`.
pub(super) fn constrain(diff: i32, threshold: i32, shift: u32) -> i32 {
    let adiff = diff.abs();
    let mag = adiff.min((threshold - (adiff >> shift)).max(0));
    if diff < 0 {
        -mag
    } else {
        mag
    }
}

/// Per-8×8 division weights compensating the differing pixel count of each
/// projected line — dav1d `div_table` (verbatim).
const DIV_TABLE: [u32; 7] = [840, 420, 280, 210, 168, 140, 120];

/// Direction search for one 8×8 luma block — dav1d `cdef_find_dir_c` (8-bit).
/// Projects the 64 pixels onto the eight CDEF directions via partial sums,
/// scores each by weighted sum-of-squares, and returns `(best_dir, var)` where
/// `best_dir ∈ 0..=7` and `var` is the directional variance
/// `(best_cost - cost[best_dir ^ 4]) >> 10`. `img[y * stride + x]` indexes the
/// block. `cost` overflows u32 by design (matches C `unsigned` wraparound), so
/// every accumulation uses wrapping arithmetic. The projection/scoring loops
/// index by mirrored offsets (`14 - n`, `n * 2 + 1`, `10 - m`) that follow the
/// reference exactly, so they stay as range loops rather than iterators.
#[allow(clippy::needless_range_loop)]
pub(super) fn cdef_find_dir(img: &[u8], stride: usize) -> (i32, u32) {
    let mut psum_hv = [[0i32; 8]; 2];
    let mut psum_diag = [[0i32; 15]; 2];
    let mut psum_alt = [[0i32; 11]; 4];

    for y in 0..8usize {
        for x in 0..8usize {
            let px = img[y * stride + x] as i32 - 128;
            psum_diag[0][y + x] += px;
            psum_alt[0][y + (x >> 1)] += px;
            psum_hv[0][y] += px;
            psum_alt[1][3 + y - (x >> 1)] += px;
            psum_diag[1][7 + y - x] += px;
            psum_alt[2][3 - (y >> 1) + x] += px;
            psum_hv[1][x] += px;
            psum_alt[3][(y >> 1) + x] += px;
        }
    }

    let mut cost = [0u32; 8];
    for n in 0..8 {
        cost[2] = cost[2].wrapping_add((psum_hv[0][n] * psum_hv[0][n]) as u32);
        cost[6] = cost[6].wrapping_add((psum_hv[1][n] * psum_hv[1][n]) as u32);
    }
    cost[2] = cost[2].wrapping_mul(105);
    cost[6] = cost[6].wrapping_mul(105);

    for n in 0..7 {
        let d = DIV_TABLE[n];
        let a0 = psum_diag[0][n] * psum_diag[0][n] + psum_diag[0][14 - n] * psum_diag[0][14 - n];
        let a4 = psum_diag[1][n] * psum_diag[1][n] + psum_diag[1][14 - n] * psum_diag[1][14 - n];
        cost[0] = cost[0].wrapping_add((a0 as u32).wrapping_mul(d));
        cost[4] = cost[4].wrapping_add((a4 as u32).wrapping_mul(d));
    }
    cost[0] = cost[0].wrapping_add((psum_diag[0][7] * psum_diag[0][7]) as u32 * 105);
    cost[4] = cost[4].wrapping_add((psum_diag[1][7] * psum_diag[1][7]) as u32 * 105);

    for n in 0..4 {
        let ci = n * 2 + 1;
        for m in 0..5 {
            cost[ci] = cost[ci].wrapping_add((psum_alt[n][3 + m] * psum_alt[n][3 + m]) as u32);
        }
        cost[ci] = cost[ci].wrapping_mul(105);
        for m in 0..3 {
            let d = DIV_TABLE[2 * m + 1];
            let a = psum_alt[n][m] * psum_alt[n][m] + psum_alt[n][10 - m] * psum_alt[n][10 - m];
            cost[ci] = cost[ci].wrapping_add((a as u32).wrapping_mul(d));
        }
    }

    let mut best_dir = 0usize;
    let mut best_cost = cost[0];
    for n in 1..8 {
        if cost[n] > best_cost {
            best_cost = cost[n];
            best_dir = n;
        }
    }
    let var = best_cost.wrapping_sub(cost[best_dir ^ 4]) >> 10;
    (best_dir as i32, var)
}

#[cfg(test)]
mod tests {
    use super::*;

    // dav1d reference (best_dir, var) for the eight harness inputs.
    include!("cdef_dir_ref.rs");

    /// Rebuild the same eight deterministic 8×8 blocks the C harness uses, then
    /// assert `cdef_find_dir` matches dav1d's (best_dir, var) bit-for-bit.
    #[test]
    fn cdef_find_dir_matches_dav1d() {
        let mut blocks = [[0u8; 64]; 8];
        for v in blocks[0].iter_mut() {
            *v = 128;
        }
        for y in 0..8usize {
            for x in 0..8usize {
                blocks[1][y * 8 + x] = (20 + x * 26) as u8;
                blocks[2][y * 8 + x] = (20 + y * 26) as u8;
                blocks[3][y * 8 + x] = (20 + (x + y) * 14) as u8;
                blocks[4][y * 8 + x] = (20 + (x + (7 - y)) * 14) as u8;
                blocks[5][y * 8 + x] = (40 + (x + (y >> 1)) * 12) as u8;
                blocks[6][y * 8 + x] = if (x ^ y) & 1 != 0 { 220 } else { 40 };
            }
        }
        let mut s: u32 = 0x12345;
        for v in blocks[7].iter_mut() {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            *v = (s >> 16) as u8;
        }

        for (b, &(want_dir, want_var)) in blocks.iter().zip(CDEF_DIR_REF.iter()) {
            let (dir, var) = cdef_find_dir(b, 8);
            assert_eq!((dir, var), (want_dir, want_var), "cdef_find_dir mismatch vs dav1d");
        }
    }

    #[test]
    fn constrain_matches_spec() {
        // apply_sign(min(|d|, max(0, thr - (|d| >> sh))), d)
        assert_eq!(constrain(0, 10, 2), 0);
        assert_eq!(constrain(3, 10, 1), 3); // |3|=3, thr-(3>>1)=10-1=9 → min(3,9)=3
        assert_eq!(constrain(-3, 10, 1), -3);
        assert_eq!(constrain(40, 10, 1), 0); // thr-(40>>1)=10-20=-10 → max(0,..)=0
        assert_eq!(constrain(-40, 10, 1), 0);
        assert_eq!(constrain(8, 10, 0), 2); // thr-(8>>0)=10-8=2 → min(8,2)=2
    }
}
