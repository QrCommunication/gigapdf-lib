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

/// Tap offsets into the 12-wide padded `tmp` buffer for each direction — dav1d
/// `dav1d_cdef_directions`. Rows 0..1 are dir 6..7, 2..9 are dir 0..7, 10..11
/// are dir 0..1, so `[dir + 2]` (primary), `[dir + 4]` (sec +2) and `[dir + 0]`
/// (sec −2) all stay in range for `dir ∈ 0..=7`.
const CDEF_DIRECTIONS: [[i32; 2]; 12] = [
    [12, 24],
    [12, 23],
    [-11, -22],
    [1, -10],
    [1, 2],
    [1, 14],
    [13, 26],
    [12, 25],
    [12, 24],
    [12, 23],
    [-11, -22],
    [1, -10],
];

const TMP_STRIDE: usize = 12;
/// Origin of the active region inside the 12×12 `tmp` buffer (2-px halo).
const TMP_ORIGIN: usize = 2 * TMP_STRIDE + 2;
/// Sentinel for unavailable edge pixels (dav1d `CDEF_VERY_LARGE` = `INT16_MIN`):
/// huge as unsigned (skipped by [`umin`]), very negative as signed (skipped by
/// `max`), and far enough that `constrain` zeroes its tap.
const CDEF_VERY_LARGE: i16 = i16::MIN;

/// Unsigned-compared minimum (dav1d `umin`), so the `CDEF_VERY_LARGE` sentinel
/// (huge when read as unsigned) never wins the running minimum.
fn umin(a: i32, b: i32) -> i32 {
    if (a as u32) < (b as u32) {
        a
    } else {
        b
    }
}

/// Floor-log2 of a positive value — dav1d `ulog2` (`31 ^ clz`). Callers gate on
/// a non-zero strength, so `x >= 1`.
fn ulog2(x: i32) -> i32 {
    31 - (x as u32).leading_zeros() as i32
}

/// Scale a primary strength by the block's directional variance — dav1d
/// `adjust_strength`. A flat block (`var == 0`) disables the primary tap set.
pub(super) fn adjust_strength(strength: i32, var: u32) -> i32 {
    if var == 0 {
        return 0;
    }
    let i = if var >> 6 != 0 { ulog2((var >> 6) as i32).min(12) } else { 0 };
    (strength * (4 + i) + 8) >> 4
}

/// Fill the 12×12 `tmp` buffer with the `w × h` block plus its 2-px halo — dav1d
/// `padding`. Available edges are copied from `src`/`left`/`top`/`bottom`;
/// unavailable ones get the `CDEF_VERY_LARGE` sentinel. `top`/`bottom` index two
/// rows of `w + 4` samples advancing by their stride; `left[y]` is the two
/// columns left of row `y`.
#[allow(clippy::too_many_arguments)]
fn cdef_padding(
    tmp: &mut [i16; 144],
    src: &[u8],
    soff: usize,
    sstride: usize,
    left: &[[u8; 2]],
    top: &[u8],
    toff: usize,
    tstride: usize,
    bottom: &[u8],
    boff: usize,
    bstride: usize,
    w: usize,
    h: usize,
    edges: u8,
) {
    let ts = TMP_STRIDE as i32;
    let origin = TMP_ORIGIN as i32;
    let idx = |y: i32, x: i32| (origin + y * ts + x) as usize;
    let (w_i, h_i) = (w as i32, h as i32);
    let mut x_start = -2i32;
    let mut x_end = w_i + 2;
    let mut y_start = -2i32;
    let mut y_end = h_i + 2;

    if edges & 0b0100 == 0 {
        // !CDEF_HAVE_TOP: rows -2..=-1, cols -2..w+1
        for r in 0..2 {
            for c in 0..w_i + 4 {
                tmp[idx(-2 + r, -2 + c)] = CDEF_VERY_LARGE;
            }
        }
        y_start = 0;
    }
    if edges & 0b1000 == 0 {
        // !CDEF_HAVE_BOTTOM: rows h..=h+1, cols -2..w+1
        for r in 0..2 {
            for c in 0..w_i + 4 {
                tmp[idx(h_i + r, -2 + c)] = CDEF_VERY_LARGE;
            }
        }
        y_end -= 2;
    }
    if edges & 0b0001 == 0 {
        // !CDEF_HAVE_LEFT: cols -2..=-1 over the (clamped) row span
        for y in y_start..y_end {
            tmp[idx(y, -2)] = CDEF_VERY_LARGE;
            tmp[idx(y, -1)] = CDEF_VERY_LARGE;
        }
        x_start = 0;
    }
    if edges & 0b0010 == 0 {
        // !CDEF_HAVE_RIGHT: cols w..=w+1 over the (clamped) row span
        for y in y_start..y_end {
            tmp[idx(y, w_i)] = CDEF_VERY_LARGE;
            tmp[idx(y, w_i + 1)] = CDEF_VERY_LARGE;
        }
        x_end -= 2;
    }

    // Top halo rows.
    for y in y_start..0 {
        let row = (y - y_start) as usize;
        for x in x_start..x_end {
            tmp[idx(y, x)] = top[(toff as i32 + row as i32 * tstride as i32 + x) as usize] as i16;
        }
    }
    // Left halo columns.
    for y in 0..h_i {
        for x in x_start..0 {
            tmp[idx(y, x)] = left[y as usize][(2 + x) as usize] as i16;
        }
    }
    // Block body.
    for y in 0..h_i {
        for x in 0..x_end {
            tmp[idx(y, x)] = src[soff + y as usize * sstride + x as usize] as i16;
        }
    }
    // Bottom halo rows.
    for y in h_i..y_end {
        let row = (y - h_i) as usize;
        for x in x_start..x_end {
            tmp[idx(y, x)] = bottom[(boff as i32 + row as i32 * bstride as i32 + x) as usize] as i16;
        }
    }
}

/// Apply the CDEF filter to one `w × h` block in place — dav1d
/// `cdef_filter_block_c`. Primary taps run along `dir`, secondary taps at
/// `dir ± 2`, each soft-thresholded by [`constrain`]; the combined case clamps
/// to the local pixel min/max while the single-strength cases store the raw
/// (wrapping) `u8`. `dst`/`top`/`bottom`/`left` must not alias (the apply pass
/// passes pre-filter copies of the halo).
///
/// The two-iteration `k` loops index `CDEF_DIRECTIONS` and drive the tap
/// weights (`pri_tap_k`, `sec_tap = 2 - k`) verbatim from the reference, so they
/// stay as range loops rather than iterators.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub(super) fn cdef_filter_block(
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
    left: &[[u8; 2]],
    top: &[u8],
    toff: usize,
    tstride: usize,
    bottom: &[u8],
    boff: usize,
    bstride: usize,
    pri_strength: i32,
    sec_strength: i32,
    dir: usize,
    damping: i32,
    w: usize,
    h: usize,
    edges: u8,
) {
    let mut tmp = [0i16; 144];
    cdef_padding(
        &mut tmp, dst, doff, dstride, left, top, toff, tstride, bottom, boff, bstride, w, h, edges,
    );
    let ts = TMP_STRIDE as i32;
    let origin = TMP_ORIGIN as i32;
    let t = |i: i32| tmp[i as usize] as i32;

    if pri_strength != 0 {
        let pri_tap0 = 4 - (pri_strength & 1); // bitdepth_min_8 == 0 for 8-bit
        let pri_shift = (damping - ulog2(pri_strength)).max(0) as u32;
        if sec_strength != 0 {
            let sec_shift = (damping - ulog2(sec_strength)).max(0) as u32;
            for y in 0..h {
                let row = origin + y as i32 * ts;
                for x in 0..w {
                    let xc = x as i32;
                    let px = dst[doff + y * dstride + x] as i32;
                    let mut sum = 0i32;
                    let (mut maxv, mut minv) = (px, px);
                    let mut pri_tap_k = pri_tap0;
                    for k in 0..2 {
                        let off1 = CDEF_DIRECTIONS[dir + 2][k];
                        let p0 = t(row + xc + off1);
                        let p1 = t(row + xc - off1);
                        sum += pri_tap_k * constrain(p0 - px, pri_strength, pri_shift);
                        sum += pri_tap_k * constrain(p1 - px, pri_strength, pri_shift);
                        pri_tap_k = (pri_tap_k & 3) | 2;
                        minv = umin(p0, minv);
                        maxv = maxv.max(p0);
                        minv = umin(p1, minv);
                        maxv = maxv.max(p1);
                        let off2 = CDEF_DIRECTIONS[dir + 4][k];
                        let off3 = CDEF_DIRECTIONS[dir][k];
                        let s0 = t(row + xc + off2);
                        let s1 = t(row + xc - off2);
                        let s2 = t(row + xc + off3);
                        let s3 = t(row + xc - off3);
                        let sec_tap = 2 - k as i32;
                        sum += sec_tap * constrain(s0 - px, sec_strength, sec_shift);
                        sum += sec_tap * constrain(s1 - px, sec_strength, sec_shift);
                        sum += sec_tap * constrain(s2 - px, sec_strength, sec_shift);
                        sum += sec_tap * constrain(s3 - px, sec_strength, sec_shift);
                        minv = umin(s0, minv);
                        maxv = maxv.max(s0);
                        minv = umin(s1, minv);
                        maxv = maxv.max(s1);
                        minv = umin(s2, minv);
                        maxv = maxv.max(s2);
                        minv = umin(s3, minv);
                        maxv = maxv.max(s3);
                    }
                    let v = px + ((sum - (sum < 0) as i32 + 8) >> 4);
                    dst[doff + y * dstride + x] = v.clamp(minv, maxv) as u8;
                }
            }
        } else {
            for y in 0..h {
                let row = origin + y as i32 * ts;
                for x in 0..w {
                    let xc = x as i32;
                    let px = dst[doff + y * dstride + x] as i32;
                    let mut sum = 0i32;
                    let mut pri_tap_k = pri_tap0;
                    for k in 0..2 {
                        let off = CDEF_DIRECTIONS[dir + 2][k];
                        let p0 = t(row + xc + off);
                        let p1 = t(row + xc - off);
                        sum += pri_tap_k * constrain(p0 - px, pri_strength, pri_shift);
                        sum += pri_tap_k * constrain(p1 - px, pri_strength, pri_shift);
                        pri_tap_k = (pri_tap_k & 3) | 2;
                    }
                    let v = px + ((sum - (sum < 0) as i32 + 8) >> 4);
                    dst[doff + y * dstride + x] = v as u8; // single-strength: wrapping u8
                }
            }
        }
    } else {
        let sec_shift = (damping - ulog2(sec_strength)).max(0) as u32;
        for y in 0..h {
            let row = origin + y as i32 * ts;
            for x in 0..w {
                let xc = x as i32;
                let px = dst[doff + y * dstride + x] as i32;
                let mut sum = 0i32;
                for k in 0..2 {
                    let off1 = CDEF_DIRECTIONS[dir + 4][k];
                    let off2 = CDEF_DIRECTIONS[dir][k];
                    let s0 = t(row + xc + off1);
                    let s1 = t(row + xc - off1);
                    let s2 = t(row + xc + off2);
                    let s3 = t(row + xc - off2);
                    let sec_tap = 2 - k as i32;
                    sum += sec_tap * constrain(s0 - px, sec_strength, sec_shift);
                    sum += sec_tap * constrain(s1 - px, sec_strength, sec_shift);
                    sum += sec_tap * constrain(s2 - px, sec_strength, sec_shift);
                    sum += sec_tap * constrain(s3 - px, sec_strength, sec_shift);
                }
                let v = px + ((sum - (sum < 0) as i32 + 8) >> 4);
                dst[doff + y * dstride + x] = v as u8; // single-strength: wrapping u8
            }
        }
    }
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

    // dav1d reference filter outputs for the harness configs.
    include!("cdef_filter_ref.rs");

    /// Rebuild the same surrounding canvas + configs the C filter harness uses
    /// (every direction, pri-only / sec-only / combined, 8×8/4×4/4×8) and assert
    /// `cdef_filter_block` reproduces dav1d's filtered block bit-for-bit. The
    /// halo (top/bottom/left) is copied out so it cannot alias the `&mut`
    /// destination — the same shape the apply pass uses.
    #[test]
    fn cdef_filter_block_matches_dav1d() {
        fn canvas_pix(x: i32, y: i32) -> u8 {
            ((x * 29 + y * 43 + (x ^ y) * 53 + 40) & 0xff) as u8
        }
        for case in CDEF_FILTER_REF {
            let (w, h) = (case.w, case.h);
            let cw = w + 4;
            let ch = h + 4;
            let mut canvas = vec![0u8; cw * ch];
            for y in 0..ch {
                for x in 0..cw {
                    canvas[y * cw + x] = canvas_pix(x as i32, y as i32);
                }
            }
            let top: Vec<u8> = canvas[0..2 * cw].to_vec();
            let bottom: Vec<u8> = canvas[(2 + h) * cw..(4 + h) * cw].to_vec();
            let left: Vec<[u8; 2]> =
                (0..h).map(|y| [canvas[(2 + y) * cw], canvas[(2 + y) * cw + 1]]).collect();
            cdef_filter_block(
                &mut canvas, 2 * cw + 2, cw, &left, &top, 2, cw, &bottom, 2, cw, case.pri,
                case.sec, case.dir, case.damping, w, h, 0b1111,
            );
            let mut got = Vec::with_capacity(w * h);
            for y in 0..h {
                for x in 0..w {
                    got.push(canvas[(2 + y) * cw + 2 + x]);
                }
            }
            assert_eq!(
                got, case.out,
                "cdef_filter_block mismatch dir={} pri={} sec={} {}x{}",
                case.dir, case.pri, case.sec, w, h
            );
        }
    }
}
