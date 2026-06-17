//! AV1 inverse transforms (DCT / IDTX / WHT) and the 2D add wrapper.
//!
//! Faithful translation of dav1d `src/itx_1d.c` + `src/itx_tmpl.c`
//! (BSD-2-Clause, © VideoLAN / Two Orioles). The 1D butterflies are fixed-point
//! and recursive (`dct8` runs `dct4` over the even lanes, etc.); the coefficient
//! rewrites such as `(3784 - 4096)` are kept verbatim — they keep intermediates
//! inside 31+sign bits on (invalid) 12-bit streams, and matter for bit-exactness.
//! ADST/FlipADST and the 64-point DCT land in a follow-up; this module covers the
//! DCT, identity and Walsh–Hadamard paths plus the 2D row/column wrapper.

#![allow(dead_code)]

use super::tile::txtp;
use super::tile::TXFM_DIMENSIONS;

/// 1D transform-type lanes (`enum Tx1dType`): row/col selector in `TX1D_TYPES`.
pub(super) const DCT_1D: u8 = 0;
pub(super) const ADST_1D: u8 = 1;
pub(super) const FLIPADST_1D: u8 = 2;
pub(super) const IDENTITY_1D: u8 = 3;

/// `inv_txfm_fn(...)` per-tx final shift, indexed by our tx order (dav1d
/// `itx_tmpl.c` macro args). 64-point sizes included for completeness.
pub(super) static ITX_SHIFT: [u8; 19] = [
    0, // TX_4X4
    1, // TX_8X8
    2, // TX_16X16
    2, // TX_32X32
    2, // TX_64X64
    0, // RTX_4X8
    0, // RTX_8X4
    1, // RTX_8X16
    1, // RTX_16X8
    1, // RTX_16X32
    1, // RTX_32X16
    1, // RTX_32X64
    1, // RTX_64X32
    1, // RTX_4X16
    1, // RTX_16X4
    2, // RTX_8X32
    2, // RTX_32X8
    2, // RTX_16X64
    2, // RTX_64X16
];

/// `dav1d_tx1d_types[txtp]` — `[row, col]` 1D transform types per `TxfmType`.
pub(super) static TX1D_TYPES: [[u8; 2]; 17] = [
    [DCT_1D, DCT_1D],             // DCT_DCT
    [ADST_1D, DCT_1D],            // ADST_DCT
    [DCT_1D, ADST_1D],            // DCT_ADST
    [ADST_1D, ADST_1D],           // ADST_ADST
    [FLIPADST_1D, DCT_1D],        // FLIPADST_DCT
    [DCT_1D, FLIPADST_1D],        // DCT_FLIPADST
    [FLIPADST_1D, FLIPADST_1D],   // FLIPADST_FLIPADST
    [ADST_1D, FLIPADST_1D],       // ADST_FLIPADST
    [FLIPADST_1D, ADST_1D],       // FLIPADST_ADST
    [IDENTITY_1D, IDENTITY_1D],   // IDTX
    [DCT_1D, IDENTITY_1D],        // V_DCT
    [IDENTITY_1D, DCT_1D],        // H_DCT
    [ADST_1D, IDENTITY_1D],       // V_ADST
    [IDENTITY_1D, ADST_1D],       // H_ADST
    [FLIPADST_1D, IDENTITY_1D],   // V_FLIPADST
    [IDENTITY_1D, FLIPADST_1D],   // H_FLIPADST
    [DCT_1D, DCT_1D],             // WHT_WHT (handled separately)
];

#[inline]
fn clip(v: i32, min: i32, max: i32) -> i32 {
    v.clamp(min, max)
}

// ---- Inverse DCT (non-64 path) -------------------------------------------

fn dct4(c: &mut [i32], s: usize, min: i32, max: i32) {
    let (in0, in1, in2, in3) = (c[0], c[s], c[2 * s], c[3 * s]);
    let t0 = ((in0 + in2) * 181 + 128) >> 8;
    let t1 = ((in0 - in2) * 181 + 128) >> 8;
    let t2 = ((in1 * 1567 - in3 * (3784 - 4096) + 2048) >> 12) - in3;
    let t3 = ((in1 * (3784 - 4096) + in3 * 1567 + 2048) >> 12) + in1;
    c[0] = clip(t0 + t3, min, max);
    c[s] = clip(t1 + t2, min, max);
    c[2 * s] = clip(t1 - t2, min, max);
    c[3 * s] = clip(t0 - t3, min, max);
}

fn dct8(c: &mut [i32], s: usize, min: i32, max: i32) {
    dct4(c, s << 1, min, max);
    let (in1, in3, in5, in7) = (c[s], c[3 * s], c[5 * s], c[7 * s]);
    let t4a = ((in1 * 799 - in7 * (4017 - 4096) + 2048) >> 12) - in7;
    let mut t5a = (in5 * 1703 - in3 * 1138 + 1024) >> 11;
    let t6a = (in5 * 1138 + in3 * 1703 + 1024) >> 11;
    let t7a = ((in1 * (4017 - 4096) + in7 * 799 + 2048) >> 12) + in1;
    let t4 = clip(t4a + t5a, min, max);
    t5a = clip(t4a - t5a, min, max);
    let t7 = clip(t7a + t6a, min, max);
    let t6a = clip(t7a - t6a, min, max);
    let t5 = ((t6a - t5a) * 181 + 128) >> 8;
    let t6 = ((t6a + t5a) * 181 + 128) >> 8;
    let (t0, t1, t2, t3) = (c[0], c[2 * s], c[4 * s], c[6 * s]);
    c[0] = clip(t0 + t7, min, max);
    c[s] = clip(t1 + t6, min, max);
    c[2 * s] = clip(t2 + t5, min, max);
    c[3 * s] = clip(t3 + t4, min, max);
    c[4 * s] = clip(t3 - t4, min, max);
    c[5 * s] = clip(t2 - t5, min, max);
    c[6 * s] = clip(t1 - t6, min, max);
    c[7 * s] = clip(t0 - t7, min, max);
}

fn dct16(c: &mut [i32], s: usize, min: i32, max: i32) {
    dct8(c, s << 1, min, max);
    let (in1, in3, in5, in7) = (c[s], c[3 * s], c[5 * s], c[7 * s]);
    let (in9, in11, in13, in15) = (c[9 * s], c[11 * s], c[13 * s], c[15 * s]);
    let mut t8a = ((in1 * 401 - in15 * (4076 - 4096) + 2048) >> 12) - in15;
    let mut t9a = (in9 * 1583 - in7 * 1299 + 1024) >> 11;
    let mut t10a = ((in5 * 1931 - in11 * (3612 - 4096) + 2048) >> 12) - in11;
    let mut t11a = ((in13 * (3920 - 4096) - in3 * 1189 + 2048) >> 12) + in13;
    let mut t12a = ((in13 * 1189 + in3 * (3920 - 4096) + 2048) >> 12) + in3;
    let mut t13a = ((in5 * (3612 - 4096) + in11 * 1931 + 2048) >> 12) + in5;
    let mut t14a = (in9 * 1299 + in7 * 1583 + 1024) >> 11;
    let mut t15a = ((in1 * (4076 - 4096) + in15 * 401 + 2048) >> 12) + in1;
    let t8 = clip(t8a + t9a, min, max);
    let mut t9 = clip(t8a - t9a, min, max);
    let mut t10 = clip(t11a - t10a, min, max);
    let mut t11 = clip(t11a + t10a, min, max);
    let mut t12 = clip(t12a + t13a, min, max);
    let mut t13 = clip(t12a - t13a, min, max);
    let mut t14 = clip(t15a - t14a, min, max);
    let t15 = clip(t15a + t14a, min, max);
    t9a = ((t14 * 1567 - t9 * (3784 - 4096) + 2048) >> 12) - t9;
    t14a = ((t14 * (3784 - 4096) + t9 * 1567 + 2048) >> 12) + t14;
    t10a = ((-(t13 * (3784 - 4096) + t10 * 1567) + 2048) >> 12) - t13;
    t13a = ((t13 * 1567 - t10 * (3784 - 4096) + 2048) >> 12) - t10;
    t8a = clip(t8 + t11, min, max);
    t9 = clip(t9a + t10a, min, max);
    t10 = clip(t9a - t10a, min, max);
    t11a = clip(t8 - t11, min, max);
    t12a = clip(t15 - t12, min, max);
    t13 = clip(t14a - t13a, min, max);
    t14 = clip(t14a + t13a, min, max);
    t15a = clip(t15 + t12, min, max);
    t10a = ((t13 - t10) * 181 + 128) >> 8;
    t13a = ((t13 + t10) * 181 + 128) >> 8;
    t11 = ((t12a - t11a) * 181 + 128) >> 8;
    t12 = ((t12a + t11a) * 181 + 128) >> 8;
    let t0 = c[0];
    let t1 = c[2 * s];
    let t2 = c[4 * s];
    let t3 = c[6 * s];
    let t4 = c[8 * s];
    let t5 = c[10 * s];
    let t6 = c[12 * s];
    let t7 = c[14 * s];
    c[0] = clip(t0 + t15a, min, max);
    c[s] = clip(t1 + t14, min, max);
    c[2 * s] = clip(t2 + t13a, min, max);
    c[3 * s] = clip(t3 + t12, min, max);
    c[4 * s] = clip(t4 + t11, min, max);
    c[5 * s] = clip(t5 + t10a, min, max);
    c[6 * s] = clip(t6 + t9, min, max);
    c[7 * s] = clip(t7 + t8a, min, max);
    c[8 * s] = clip(t7 - t8a, min, max);
    c[9 * s] = clip(t6 - t9, min, max);
    c[10 * s] = clip(t5 - t10a, min, max);
    c[11 * s] = clip(t4 - t11, min, max);
    c[12 * s] = clip(t3 - t12, min, max);
    c[13 * s] = clip(t2 - t13a, min, max);
    c[14 * s] = clip(t1 - t14, min, max);
    c[15 * s] = clip(t0 - t15a, min, max);
}

fn dct32(c: &mut [i32], s: usize, min: i32, max: i32) {
    dct16(c, s << 1, min, max);
    let (in1, in3, in5, in7) = (c[s], c[3 * s], c[5 * s], c[7 * s]);
    let (in9, in11, in13, in15) = (c[9 * s], c[11 * s], c[13 * s], c[15 * s]);
    let (in17, in19, in21, in23) = (c[17 * s], c[19 * s], c[21 * s], c[23 * s]);
    let (in25, in27, in29, in31) = (c[25 * s], c[27 * s], c[29 * s], c[31 * s]);
    let mut t16a = ((in1 * 201 - in31 * (4091 - 4096) + 2048) >> 12) - in31;
    let mut t17a = ((in17 * (3035 - 4096) - in15 * 2751 + 2048) >> 12) + in17;
    let mut t18a = ((in9 * 1751 - in23 * (3703 - 4096) + 2048) >> 12) - in23;
    let mut t19a = ((in25 * (3857 - 4096) - in7 * 1380 + 2048) >> 12) + in25;
    let mut t20a = ((in5 * 995 - in27 * (3973 - 4096) + 2048) >> 12) - in27;
    let mut t21a = ((in21 * (3513 - 4096) - in11 * 2106 + 2048) >> 12) + in21;
    let mut t22a = (in13 * 1220 - in19 * 1645 + 1024) >> 11;
    let mut t23a = ((in29 * (4052 - 4096) - in3 * 601 + 2048) >> 12) + in29;
    let mut t24a = ((in29 * 601 + in3 * (4052 - 4096) + 2048) >> 12) + in3;
    let mut t25a = (in13 * 1645 + in19 * 1220 + 1024) >> 11;
    let mut t26a = ((in21 * 2106 + in11 * (3513 - 4096) + 2048) >> 12) + in11;
    let mut t27a = ((in5 * (3973 - 4096) + in27 * 995 + 2048) >> 12) + in5;
    let mut t28a = ((in25 * 1380 + in7 * (3857 - 4096) + 2048) >> 12) + in7;
    let mut t29a = ((in9 * (3703 - 4096) + in23 * 1751 + 2048) >> 12) + in9;
    let mut t30a = ((in17 * 2751 + in15 * (3035 - 4096) + 2048) >> 12) + in15;
    let mut t31a = ((in1 * (4091 - 4096) + in31 * 201 + 2048) >> 12) + in1;
    let mut t16 = clip(t16a + t17a, min, max);
    let mut t17 = clip(t16a - t17a, min, max);
    let mut t18 = clip(t19a - t18a, min, max);
    let mut t19 = clip(t19a + t18a, min, max);
    let mut t20 = clip(t20a + t21a, min, max);
    let mut t21 = clip(t20a - t21a, min, max);
    let mut t22 = clip(t23a - t22a, min, max);
    let mut t23 = clip(t23a + t22a, min, max);
    let mut t24 = clip(t24a + t25a, min, max);
    let mut t25 = clip(t24a - t25a, min, max);
    let mut t26 = clip(t27a - t26a, min, max);
    let mut t27 = clip(t27a + t26a, min, max);
    let mut t28 = clip(t28a + t29a, min, max);
    let mut t29 = clip(t28a - t29a, min, max);
    let mut t30 = clip(t31a - t30a, min, max);
    let mut t31 = clip(t31a + t30a, min, max);
    t17a = ((t30 * 799 - t17 * (4017 - 4096) + 2048) >> 12) - t17;
    t30a = ((t30 * (4017 - 4096) + t17 * 799 + 2048) >> 12) + t30;
    t18a = ((-(t29 * (4017 - 4096) + t18 * 799) + 2048) >> 12) - t29;
    t29a = ((t29 * 799 - t18 * (4017 - 4096) + 2048) >> 12) - t18;
    t21a = (t26 * 1703 - t21 * 1138 + 1024) >> 11;
    t26a = (t26 * 1138 + t21 * 1703 + 1024) >> 11;
    t22a = (-(t25 * 1138 + t22 * 1703) + 1024) >> 11;
    t25a = (t25 * 1703 - t22 * 1138 + 1024) >> 11;
    t16a = clip(t16 + t19, min, max);
    t17 = clip(t17a + t18a, min, max);
    t18 = clip(t17a - t18a, min, max);
    t19a = clip(t16 - t19, min, max);
    t20a = clip(t23 - t20, min, max);
    t21 = clip(t22a - t21a, min, max);
    t22 = clip(t22a + t21a, min, max);
    t23a = clip(t23 + t20, min, max);
    t24a = clip(t24 + t27, min, max);
    t25 = clip(t25a + t26a, min, max);
    t26 = clip(t25a - t26a, min, max);
    t27a = clip(t24 - t27, min, max);
    t28a = clip(t31 - t28, min, max);
    t29 = clip(t30a - t29a, min, max);
    t30 = clip(t30a + t29a, min, max);
    t31a = clip(t31 + t28, min, max);
    t18a = ((t29 * 1567 - t18 * (3784 - 4096) + 2048) >> 12) - t18;
    t29a = ((t29 * (3784 - 4096) + t18 * 1567 + 2048) >> 12) + t29;
    t19 = ((t28a * 1567 - t19a * (3784 - 4096) + 2048) >> 12) - t19a;
    t28 = ((t28a * (3784 - 4096) + t19a * 1567 + 2048) >> 12) + t28a;
    t20 = ((-(t27a * (3784 - 4096) + t20a * 1567) + 2048) >> 12) - t27a;
    t27 = ((t27a * 1567 - t20a * (3784 - 4096) + 2048) >> 12) - t20a;
    t21a = ((-(t26 * (3784 - 4096) + t21 * 1567) + 2048) >> 12) - t26;
    t26a = ((t26 * 1567 - t21 * (3784 - 4096) + 2048) >> 12) - t21;
    t16 = clip(t16a + t23a, min, max);
    t17a = clip(t17 + t22, min, max);
    t18 = clip(t18a + t21a, min, max);
    t19a = clip(t19 + t20, min, max);
    t20a = clip(t19 - t20, min, max);
    t21 = clip(t18a - t21a, min, max);
    t22a = clip(t17 - t22, min, max);
    t23 = clip(t16a - t23a, min, max);
    t24 = clip(t31a - t24a, min, max);
    t25a = clip(t30 - t25, min, max);
    t26 = clip(t29a - t26a, min, max);
    t27a = clip(t28 - t27, min, max);
    t28a = clip(t28 + t27, min, max);
    t29 = clip(t29a + t26a, min, max);
    t30a = clip(t30 + t25, min, max);
    t31 = clip(t31a + t24a, min, max);
    t20 = ((t27a - t20a) * 181 + 128) >> 8;
    t27 = ((t27a + t20a) * 181 + 128) >> 8;
    t21a = ((t26 - t21) * 181 + 128) >> 8;
    t26a = ((t26 + t21) * 181 + 128) >> 8;
    t22 = ((t25a - t22a) * 181 + 128) >> 8;
    t25 = ((t25a + t22a) * 181 + 128) >> 8;
    t23a = ((t24 - t23) * 181 + 128) >> 8;
    t24a = ((t24 + t23) * 181 + 128) >> 8;
    let tt = [
        c[0],
        c[2 * s],
        c[4 * s],
        c[6 * s],
        c[8 * s],
        c[10 * s],
        c[12 * s],
        c[14 * s],
        c[16 * s],
        c[18 * s],
        c[20 * s],
        c[22 * s],
        c[24 * s],
        c[26 * s],
        c[28 * s],
        c[30 * s],
    ];
    let hi = [
        t31, t30a, t29, t28a, t27, t26a, t25, t24a, t23a, t22, t21a, t20, t19a, t18, t17a, t16,
    ];
    for i in 0..16 {
        c[i * s] = clip(tt[i] + hi[i], min, max);
        c[(31 - i) * s] = clip(tt[i] - hi[i], min, max);
    }
}

// ---- Identity ------------------------------------------------------------

fn identity4(c: &mut [i32], s: usize, _min: i32, _max: i32) {
    for i in 0..4 {
        let v = c[i * s];
        c[i * s] = v + ((v * 1697 + 2048) >> 12);
    }
}

fn identity8(c: &mut [i32], s: usize, _min: i32, _max: i32) {
    for i in 0..8 {
        c[i * s] *= 2;
    }
}

fn identity16(c: &mut [i32], s: usize, _min: i32, _max: i32) {
    for i in 0..16 {
        let v = c[i * s];
        c[i * s] = 2 * v + ((v * 1697 + 1024) >> 11);
    }
}

fn identity32(c: &mut [i32], s: usize, _min: i32, _max: i32) {
    for i in 0..32 {
        c[i * s] *= 4;
    }
}

/// `dav1d_inv_wht4_1d_c` — the lossless Walsh–Hadamard 4-point inverse.
fn wht4(c: &mut [i32], s: usize) {
    let (in0, in1, in2, in3) = (c[0], c[s], c[2 * s], c[3 * s]);
    let t0 = in0 + in1;
    let t2 = in2 - in3;
    let t4 = (t0 - t2) >> 1;
    let t3 = t4 - in3;
    let t1 = t4 - in1;
    c[0] = t0 - t3;
    c[s] = t3;
    c[2 * s] = t1;
    c[3 * s] = t2 + t1;
}

/// Dispatch a 1D inverse transform of `len_log2` (`0=4 .. 3=32`) and type.
/// ADST/FlipADST are not yet implemented (DCT + identity only).
fn itx_1d(c: &mut [i32], s: usize, len_log2: usize, ty: u8, min: i32, max: i32) {
    match ty {
        DCT_1D => match len_log2 {
            0 => dct4(c, s, min, max),
            1 => dct8(c, s, min, max),
            2 => dct16(c, s, min, max),
            _ => dct32(c, s, min, max),
        },
        IDENTITY_1D => match len_log2 {
            0 => identity4(c, s, min, max),
            1 => identity8(c, s, min, max),
            2 => identity16(c, s, min, max),
            _ => identity32(c, s, min, max),
        },
        // ADST / FlipADST land in the next layer; never reached by the DCT/IDTX
        // transform types wired so far.
        _ => unimplemented!("ADST/FlipADST inverse not yet implemented"),
    }
}

/// 2D inverse transform of dequantized coefficients `coeff` (column-major,
/// `coeff[y + x*sh]`), returning the row-major residual block (`w*h`) the caller
/// adds to the intra predictor (`dst = clip_pixel(dst + residual)`). Mirrors
/// dav1d `inv_txfm_add_c` for ≤32-point transforms (64-point follows). `eob` is
/// the last-nonzero scan index; `txtp` selects the row/col 1D pair.
pub(super) fn inv_txfm_residual(coeff: &mut [i32], tx: usize, txtp: u8, eob: i32) -> Vec<i32> {
    let lw = TXFM_DIMENSIONS[tx][0] as usize;
    let lh = TXFM_DIMENSIONS[tx][1] as usize;
    let w = 4usize << lw;
    let h = 4usize << lh;
    let shift = ITX_SHIFT[tx] as i32;
    let is_rect2 = w * 2 == h || h * 2 == w;
    let rnd = (1i32 << shift) >> 1;
    let mut out = vec![0i32; w * h];

    let has_dconly = (txtp == txtp::DCT_DCT) as i32;
    if eob < has_dconly {
        let mut dc = coeff[0];
        coeff[0] = 0;
        if is_rect2 {
            dc = (dc * 181 + 128) >> 8;
        }
        dc = (dc * 181 + 128) >> 8;
        dc = (dc + rnd) >> shift;
        dc = (dc * 181 + 128 + 2048) >> 12;
        for v in out.iter_mut() {
            *v = dc;
        }
        return out;
    }

    let types = TX1D_TYPES[txtp as usize];
    let sh = h.min(32);
    let sw = w.min(32);
    let row_clip_min = i16::MIN as i32;
    let row_clip_max = !row_clip_min;
    let col_clip_min = i16::MIN as i32;
    let col_clip_max = !col_clip_min;

    // Row pass: every row up to `sh` (rows with all-zero input transform to
    // zero, so processing the full set matches dav1d's last-nonzero shortcut).
    let mut tmp = vec![0i32; w * sh];
    for y in 0..sh {
        let row = &mut tmp[y * w..y * w + w];
        if is_rect2 {
            for (x, slot) in row.iter_mut().enumerate().take(sw) {
                *slot = (coeff[y + x * sh] * 181 + 128) >> 8;
            }
        } else {
            for (x, slot) in row.iter_mut().enumerate().take(sw) {
                *slot = coeff[y + x * sh];
            }
        }
        itx_1d(row, 1, lw, types[0], row_clip_min, row_clip_max);
    }

    // Intermediate round + shift + clip.
    for v in tmp.iter_mut() {
        *v = clip((*v + rnd) >> shift, col_clip_min, col_clip_max);
    }

    // Column pass (stride = w).
    for x in 0..w {
        itx_1d(&mut tmp[x..], w, lh, types[1], col_clip_min, col_clip_max);
    }

    for (o, t) in out.iter_mut().zip(tmp.iter()) {
        *o = (*t + 8) >> 4;
    }
    // Clear the consumed coefficients (dav1d zeroes them for the next block).
    for v in coeff.iter_mut().take(sw * sh) {
        *v = 0;
    }
    out
}

/// 2D inverse Walsh–Hadamard (lossless 4×4), returning the residual block.
pub(super) fn inv_wht4x4_residual(coeff: &mut [i32]) -> Vec<i32> {
    let mut tmp = [0i32; 16];
    for y in 0..4 {
        for x in 0..4 {
            tmp[y * 4 + x] = coeff[y + x * 4] >> 2;
        }
        wht4(&mut tmp[y * 4..], 1);
    }
    for x in 0..4 {
        wht4(&mut tmp[x..], 4);
    }
    for v in coeff.iter_mut().take(16) {
        *v = 0;
    }
    tmp.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("itx_ref.rs");

    fn input(n: usize) -> Vec<i32> {
        (0..n).map(|i| ((i as i32 * 37) % 97) - 48).collect()
    }

    #[test]
    fn dct_idtx_wht_match_dav1d_reference() {
        let (mn, mx) = (-32768, 32767);
        let cases: [(usize, u8, usize); 9] = [
            (4, DCT_1D, 0),
            (8, DCT_1D, 1),
            (16, DCT_1D, 2),
            (32, DCT_1D, 3),
            (4, IDENTITY_1D, 0),
            (8, IDENTITY_1D, 1),
            (16, IDENTITY_1D, 2),
            (32, IDENTITY_1D, 3),
            (4, 255, 0), // WHT sentinel
        ];
        for (idx, &(n, ty, ll)) in cases.iter().enumerate() {
            let mut c = input(n);
            if ty == 255 {
                wht4(&mut c, 1);
            } else {
                itx_1d(&mut c, 1, ll, ty, mn, mx);
            }
            assert_eq!(c, ITX_REF[idx], "1D transform case {idx} (n={n}, ty={ty})");
        }
    }

    #[test]
    fn inv_txfm_2d_dc_only_is_uniform() {
        // DCT_DCT with eob 0 takes the DC-only fast path → a flat residual.
        let mut cf = vec![0i32; 64];
        cf[0] = 1000;
        let res = inv_txfm_residual(&mut cf, 1, txtp::DCT_DCT, 0); // 8×8
        assert_eq!(res.len(), 64);
        assert!(res.iter().all(|&v| v == res[0]), "DC-only residual not flat");
        assert_eq!(cf[0], 0, "DC coefficient not cleared");
    }

    #[test]
    fn inv_txfm_2d_full_runs() {
        // Full 8×8 DCT_DCT path with a spread of coefficients: must run and
        // produce a finite residual of the right size.
        let mut cf: Vec<i32> = (0..64).map(|i| ((i * 11) % 23) as i32 - 11).collect();
        let res = inv_txfm_residual(&mut cf, 1, txtp::DCT_DCT, 20);
        assert_eq!(res.len(), 64);
        assert!(res.iter().all(|&v| v.abs() < 1 << 20));
    }
}
