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

// Each `dct{N}` takes a `tx64` flag: when set, this transform is the even-lane
// sub-DCT of a larger 64-point transform, so only the lower inputs are read and
// the first-stage twiddles use the half-angle constants (dav1d's `tx64` branch).
fn dct4(c: &mut [i32], s: usize, min: i32, max: i32, tx64: bool) {
    let in0 = c[0];
    let in1 = c[s];
    let (t0, t1, t2, t3);
    if tx64 {
        t0 = (in0 * 181 + 128) >> 8;
        t1 = t0;
        t2 = (in1 * 1567 + 2048) >> 12;
        t3 = (in1 * 3784 + 2048) >> 12;
    } else {
        let in2 = c[2 * s];
        let in3 = c[3 * s];
        t0 = ((in0 + in2) * 181 + 128) >> 8;
        t1 = ((in0 - in2) * 181 + 128) >> 8;
        t2 = ((in1 * 1567 - in3 * (3784 - 4096) + 2048) >> 12) - in3;
        t3 = ((in1 * (3784 - 4096) + in3 * 1567 + 2048) >> 12) + in1;
    }
    c[0] = clip(t0 + t3, min, max);
    c[s] = clip(t1 + t2, min, max);
    c[2 * s] = clip(t1 - t2, min, max);
    c[3 * s] = clip(t0 - t3, min, max);
}

fn dct8(c: &mut [i32], s: usize, min: i32, max: i32, tx64: bool) {
    dct4(c, s << 1, min, max, tx64);
    let in1 = c[s];
    let in3 = c[3 * s];
    let (t4a, mut t5a, t6a, t7a);
    if tx64 {
        t4a = (in1 * 799 + 2048) >> 12;
        t5a = (in3 * -2276 + 2048) >> 12;
        t6a = (in3 * 3406 + 2048) >> 12;
        t7a = (in1 * 4017 + 2048) >> 12;
    } else {
        let in5 = c[5 * s];
        let in7 = c[7 * s];
        t4a = ((in1 * 799 - in7 * (4017 - 4096) + 2048) >> 12) - in7;
        t5a = (in5 * 1703 - in3 * 1138 + 1024) >> 11;
        t6a = (in5 * 1138 + in3 * 1703 + 1024) >> 11;
        t7a = ((in1 * (4017 - 4096) + in7 * 799 + 2048) >> 12) + in1;
    }
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

fn dct16(c: &mut [i32], s: usize, min: i32, max: i32, tx64: bool) {
    dct8(c, s << 1, min, max, tx64);
    let (in1, in3, in5, in7) = (c[s], c[3 * s], c[5 * s], c[7 * s]);
    let (mut t8a, mut t9a, mut t10a, mut t11a);
    let (mut t12a, mut t13a, mut t14a, mut t15a);
    if tx64 {
        t8a = (in1 * 401 + 2048) >> 12;
        t9a = (in7 * -2598 + 2048) >> 12;
        t10a = (in5 * 1931 + 2048) >> 12;
        t11a = (in3 * -1189 + 2048) >> 12;
        t12a = (in3 * 3920 + 2048) >> 12;
        t13a = (in5 * 3612 + 2048) >> 12;
        t14a = (in7 * 3166 + 2048) >> 12;
        t15a = (in1 * 4076 + 2048) >> 12;
    } else {
        let (in9, in11, in13, in15) = (c[9 * s], c[11 * s], c[13 * s], c[15 * s]);
        t8a = ((in1 * 401 - in15 * (4076 - 4096) + 2048) >> 12) - in15;
        t9a = (in9 * 1583 - in7 * 1299 + 1024) >> 11;
        t10a = ((in5 * 1931 - in11 * (3612 - 4096) + 2048) >> 12) - in11;
        t11a = ((in13 * (3920 - 4096) - in3 * 1189 + 2048) >> 12) + in13;
        t12a = ((in13 * 1189 + in3 * (3920 - 4096) + 2048) >> 12) + in3;
        t13a = ((in5 * (3612 - 4096) + in11 * 1931 + 2048) >> 12) + in5;
        t14a = (in9 * 1299 + in7 * 1583 + 1024) >> 11;
        t15a = ((in1 * (4076 - 4096) + in15 * 401 + 2048) >> 12) + in1;
    }
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

fn dct32(c: &mut [i32], s: usize, min: i32, max: i32, tx64: bool) {
    dct16(c, s << 1, min, max, tx64);
    let (in1, in3, in5, in7) = (c[s], c[3 * s], c[5 * s], c[7 * s]);
    let (in9, in11, in13, in15) = (c[9 * s], c[11 * s], c[13 * s], c[15 * s]);
    let (mut t16a, mut t17a, mut t18a, mut t19a);
    let (mut t20a, mut t21a, mut t22a, mut t23a);
    let (mut t24a, mut t25a, mut t26a, mut t27a);
    let (mut t28a, mut t29a, mut t30a, mut t31a);
    if tx64 {
        t16a = (in1 * 201 + 2048) >> 12;
        t17a = (in15 * -2751 + 2048) >> 12;
        t18a = (in9 * 1751 + 2048) >> 12;
        t19a = (in7 * -1380 + 2048) >> 12;
        t20a = (in5 * 995 + 2048) >> 12;
        t21a = (in11 * -2106 + 2048) >> 12;
        t22a = (in13 * 2440 + 2048) >> 12;
        t23a = (in3 * -601 + 2048) >> 12;
        t24a = (in3 * 4052 + 2048) >> 12;
        t25a = (in13 * 3290 + 2048) >> 12;
        t26a = (in11 * 3513 + 2048) >> 12;
        t27a = (in5 * 3973 + 2048) >> 12;
        t28a = (in7 * 3857 + 2048) >> 12;
        t29a = (in9 * 3703 + 2048) >> 12;
        t30a = (in15 * 3035 + 2048) >> 12;
        t31a = (in1 * 4091 + 2048) >> 12;
    } else {
        let (in17, in19, in21, in23) = (c[17 * s], c[19 * s], c[21 * s], c[23 * s]);
        let (in25, in27, in29, in31) = (c[25 * s], c[27 * s], c[29 * s], c[31 * s]);
        t16a = ((in1 * 201 - in31 * (4091 - 4096) + 2048) >> 12) - in31;
        t17a = ((in17 * (3035 - 4096) - in15 * 2751 + 2048) >> 12) + in17;
        t18a = ((in9 * 1751 - in23 * (3703 - 4096) + 2048) >> 12) - in23;
        t19a = ((in25 * (3857 - 4096) - in7 * 1380 + 2048) >> 12) + in25;
        t20a = ((in5 * 995 - in27 * (3973 - 4096) + 2048) >> 12) - in27;
        t21a = ((in21 * (3513 - 4096) - in11 * 2106 + 2048) >> 12) + in21;
        t22a = (in13 * 1220 - in19 * 1645 + 1024) >> 11;
        t23a = ((in29 * (4052 - 4096) - in3 * 601 + 2048) >> 12) + in29;
        t24a = ((in29 * 601 + in3 * (4052 - 4096) + 2048) >> 12) + in3;
        t25a = (in13 * 1645 + in19 * 1220 + 1024) >> 11;
        t26a = ((in21 * 2106 + in11 * (3513 - 4096) + 2048) >> 12) + in11;
        t27a = ((in5 * (3973 - 4096) + in27 * 995 + 2048) >> 12) + in5;
        t28a = ((in25 * 1380 + in7 * (3857 - 4096) + 2048) >> 12) + in7;
        t29a = ((in9 * (3703 - 4096) + in23 * 1751 + 2048) >> 12) + in9;
        t30a = ((in17 * 2751 + in15 * (3035 - 4096) + 2048) >> 12) + in15;
        t31a = ((in1 * (4091 - 4096) + in31 * 201 + 2048) >> 12) + in1;
    }
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

/// Inverse 64-point DCT (dav1d `inv_dct64_1d_c`): the even half is a 32-point DCT
/// on the doubled-stride lanes (with `tx64`), then this odd half (t32..t63) is
/// folded in. Faithful line-by-line transcription of the butterfly network.
#[allow(clippy::needless_range_loop)]
fn dct64(c: &mut [i32], s: usize, min: i32, max: i32) {
    dct32(c, s << 1, min, max, true);
    let (in1, in3, in5, in7) = (c[s], c[3 * s], c[5 * s], c[7 * s]);
    let (in9, in11, in13, in15) = (c[9 * s], c[11 * s], c[13 * s], c[15 * s]);
    let (in17, in19, in21, in23) = (c[17 * s], c[19 * s], c[21 * s], c[23 * s]);
    let (in25, in27, in29, in31) = (c[25 * s], c[27 * s], c[29 * s], c[31 * s]);

    let mut t32a = (in1 * 101 + 2048) >> 12;
    let mut t33a = (in31 * -2824 + 2048) >> 12;
    let mut t34a = (in17 * 1660 + 2048) >> 12;
    let mut t35a = (in15 * -1474 + 2048) >> 12;
    let mut t36a = (in9 * 897 + 2048) >> 12;
    let mut t37a = (in23 * -2191 + 2048) >> 12;
    let mut t38a = (in25 * 2359 + 2048) >> 12;
    let mut t39a = (in7 * -700 + 2048) >> 12;
    let mut t40a = (in5 * 501 + 2048) >> 12;
    let mut t41a = (in27 * -2520 + 2048) >> 12;
    let mut t42a = (in21 * 2019 + 2048) >> 12;
    let mut t43a = (in11 * -1092 + 2048) >> 12;
    let mut t44a = (in13 * 1285 + 2048) >> 12;
    let mut t45a = (in19 * -1842 + 2048) >> 12;
    let mut t46a = (in29 * 2675 + 2048) >> 12;
    let mut t47a = (in3 * -301 + 2048) >> 12;
    let mut t48a = (in3 * 4085 + 2048) >> 12;
    let mut t49a = (in29 * 3102 + 2048) >> 12;
    let mut t50a = (in19 * 3659 + 2048) >> 12;
    let mut t51a = (in13 * 3889 + 2048) >> 12;
    let mut t52a = (in11 * 3948 + 2048) >> 12;
    let mut t53a = (in21 * 3564 + 2048) >> 12;
    let mut t54a = (in27 * 3229 + 2048) >> 12;
    let mut t55a = (in5 * 4065 + 2048) >> 12;
    let mut t56a = (in7 * 4036 + 2048) >> 12;
    let mut t57a = (in25 * 3349 + 2048) >> 12;
    let mut t58a = (in23 * 3461 + 2048) >> 12;
    let mut t59a = (in9 * 3996 + 2048) >> 12;
    let mut t60a = (in15 * 3822 + 2048) >> 12;
    let mut t61a = (in17 * 3745 + 2048) >> 12;
    let mut t62a = (in31 * 2967 + 2048) >> 12;
    let mut t63a = (in1 * 4095 + 2048) >> 12;

    let mut t32 = clip(t32a + t33a, min, max);
    let mut t33 = clip(t32a - t33a, min, max);
    let mut t34 = clip(t35a - t34a, min, max);
    let mut t35 = clip(t35a + t34a, min, max);
    let mut t36 = clip(t36a + t37a, min, max);
    let mut t37 = clip(t36a - t37a, min, max);
    let mut t38 = clip(t39a - t38a, min, max);
    let mut t39 = clip(t39a + t38a, min, max);
    let mut t40 = clip(t40a + t41a, min, max);
    let mut t41 = clip(t40a - t41a, min, max);
    let mut t42 = clip(t43a - t42a, min, max);
    let mut t43 = clip(t43a + t42a, min, max);
    let mut t44 = clip(t44a + t45a, min, max);
    let mut t45 = clip(t44a - t45a, min, max);
    let mut t46 = clip(t47a - t46a, min, max);
    let mut t47 = clip(t47a + t46a, min, max);
    let mut t48 = clip(t48a + t49a, min, max);
    let mut t49 = clip(t48a - t49a, min, max);
    let mut t50 = clip(t51a - t50a, min, max);
    let mut t51 = clip(t51a + t50a, min, max);
    let mut t52 = clip(t52a + t53a, min, max);
    let mut t53 = clip(t52a - t53a, min, max);
    let mut t54 = clip(t55a - t54a, min, max);
    let mut t55 = clip(t55a + t54a, min, max);
    let mut t56 = clip(t56a + t57a, min, max);
    let mut t57 = clip(t56a - t57a, min, max);
    let mut t58 = clip(t59a - t58a, min, max);
    let mut t59 = clip(t59a + t58a, min, max);
    let mut t60 = clip(t60a + t61a, min, max);
    let mut t61 = clip(t60a - t61a, min, max);
    let mut t62 = clip(t63a - t62a, min, max);
    let mut t63 = clip(t63a + t62a, min, max);

    t33a = ((t33 * (4096 - 4076) + t62 * 401 + 2048) >> 12) - t33;
    t34a = ((t34 * -401 + t61 * (4096 - 4076) + 2048) >> 12) - t61;
    t37a = (t37 * -1299 + t58 * 1583 + 1024) >> 11;
    t38a = (t38 * -1583 + t57 * -1299 + 1024) >> 11;
    t41a = ((t41 * (4096 - 3612) + t54 * 1931 + 2048) >> 12) - t41;
    t42a = ((t42 * -1931 + t53 * (4096 - 3612) + 2048) >> 12) - t53;
    t45a = ((t45 * -1189 + t50 * (3920 - 4096) + 2048) >> 12) + t50;
    t46a = ((t46 * (4096 - 3920) + t49 * -1189 + 2048) >> 12) - t46;
    t49a = ((t46 * -1189 + t49 * (3920 - 4096) + 2048) >> 12) + t49;
    t50a = ((t45 * (3920 - 4096) + t50 * 1189 + 2048) >> 12) + t45;
    t53a = ((t42 * (4096 - 3612) + t53 * 1931 + 2048) >> 12) - t42;
    t54a = ((t41 * 1931 + t54 * (3612 - 4096) + 2048) >> 12) + t54;
    t57a = (t38 * -1299 + t57 * 1583 + 1024) >> 11;
    t58a = (t37 * 1583 + t58 * 1299 + 1024) >> 11;
    t61a = ((t34 * (4096 - 4076) + t61 * 401 + 2048) >> 12) - t34;
    t62a = ((t33 * 401 + t62 * (4076 - 4096) + 2048) >> 12) + t62;

    t32a = clip(t32 + t35, min, max);
    t33 = clip(t33a + t34a, min, max);
    t34 = clip(t33a - t34a, min, max);
    t35a = clip(t32 - t35, min, max);
    t36a = clip(t39 - t36, min, max);
    t37 = clip(t38a - t37a, min, max);
    t38 = clip(t38a + t37a, min, max);
    t39a = clip(t39 + t36, min, max);
    t40a = clip(t40 + t43, min, max);
    t41 = clip(t41a + t42a, min, max);
    t42 = clip(t41a - t42a, min, max);
    t43a = clip(t40 - t43, min, max);
    t44a = clip(t47 - t44, min, max);
    t45 = clip(t46a - t45a, min, max);
    t46 = clip(t46a + t45a, min, max);
    t47a = clip(t47 + t44, min, max);
    t48a = clip(t48 + t51, min, max);
    t49 = clip(t49a + t50a, min, max);
    t50 = clip(t49a - t50a, min, max);
    t51a = clip(t48 - t51, min, max);
    t52a = clip(t55 - t52, min, max);
    t53 = clip(t54a - t53a, min, max);
    t54 = clip(t54a + t53a, min, max);
    t55a = clip(t55 + t52, min, max);
    t56a = clip(t56 + t59, min, max);
    t57 = clip(t57a + t58a, min, max);
    t58 = clip(t57a - t58a, min, max);
    t59a = clip(t56 - t59, min, max);
    t60a = clip(t63 - t60, min, max);
    t61 = clip(t62a - t61a, min, max);
    t62 = clip(t62a + t61a, min, max);
    t63a = clip(t63 + t60, min, max);

    t34a = ((t34 * (4096 - 4017) + t61 * 799 + 2048) >> 12) - t34;
    t35 = ((t35a * (4096 - 4017) + t60a * 799 + 2048) >> 12) - t35a;
    t36 = ((t36a * -799 + t59a * (4096 - 4017) + 2048) >> 12) - t59a;
    t37a = ((t37 * -799 + t58 * (4096 - 4017) + 2048) >> 12) - t58;
    t42a = (t42 * -1138 + t53 * 1703 + 1024) >> 11;
    t43 = (t43a * -1138 + t52a * 1703 + 1024) >> 11;
    t44 = (t44a * -1703 + t51a * -1138 + 1024) >> 11;
    t45a = (t45 * -1703 + t50 * -1138 + 1024) >> 11;
    t50a = (t45 * -1138 + t50 * 1703 + 1024) >> 11;
    t51 = (t44a * -1138 + t51a * 1703 + 1024) >> 11;
    t52 = (t43a * 1703 + t52a * 1138 + 1024) >> 11;
    t53a = (t42 * 1703 + t53 * 1138 + 1024) >> 11;
    t58a = ((t37 * (4096 - 4017) + t58 * 799 + 2048) >> 12) - t37;
    t59 = ((t36a * (4096 - 4017) + t59a * 799 + 2048) >> 12) - t36a;
    t60 = ((t35a * 799 + t60a * (4017 - 4096) + 2048) >> 12) + t60a;
    t61a = ((t34 * 799 + t61 * (4017 - 4096) + 2048) >> 12) + t61;

    t32 = clip(t32a + t39a, min, max);
    t33a = clip(t33 + t38, min, max);
    t34 = clip(t34a + t37a, min, max);
    t35a = clip(t35 + t36, min, max);
    t36a = clip(t35 - t36, min, max);
    t37 = clip(t34a - t37a, min, max);
    t38a = clip(t33 - t38, min, max);
    t39 = clip(t32a - t39a, min, max);
    t40 = clip(t47a - t40a, min, max);
    t41a = clip(t46 - t41, min, max);
    t42 = clip(t45a - t42a, min, max);
    t43a = clip(t44 - t43, min, max);
    t44a = clip(t44 + t43, min, max);
    t45 = clip(t45a + t42a, min, max);
    t46a = clip(t46 + t41, min, max);
    t47 = clip(t47a + t40a, min, max);
    t48 = clip(t48a + t55a, min, max);
    t49a = clip(t49 + t54, min, max);
    t50 = clip(t50a + t53a, min, max);
    t51a = clip(t51 + t52, min, max);
    t52a = clip(t51 - t52, min, max);
    t53 = clip(t50a - t53a, min, max);
    t54a = clip(t49 - t54, min, max);
    t55 = clip(t48a - t55a, min, max);
    t56 = clip(t63a - t56a, min, max);
    t57a = clip(t62 - t57, min, max);
    t58 = clip(t61a - t58a, min, max);
    t59a = clip(t60 - t59, min, max);
    t60a = clip(t60 + t59, min, max);
    t61 = clip(t61a + t58a, min, max);
    t62a = clip(t62 + t57, min, max);
    t63 = clip(t63a + t56a, min, max);

    t36 = ((t36a * (4096 - 3784) + t59a * 1567 + 2048) >> 12) - t36a;
    t37a = ((t37 * (4096 - 3784) + t58 * 1567 + 2048) >> 12) - t37;
    t38 = ((t38a * (4096 - 3784) + t57a * 1567 + 2048) >> 12) - t38a;
    t39a = ((t39 * (4096 - 3784) + t56 * 1567 + 2048) >> 12) - t39;
    t40a = ((t40 * -1567 + t55 * (4096 - 3784) + 2048) >> 12) - t55;
    t41 = ((t41a * -1567 + t54a * (4096 - 3784) + 2048) >> 12) - t54a;
    t42a = ((t42 * -1567 + t53 * (4096 - 3784) + 2048) >> 12) - t53;
    t43 = ((t43a * -1567 + t52a * (4096 - 3784) + 2048) >> 12) - t52a;
    t52 = ((t43a * (4096 - 3784) + t52a * 1567 + 2048) >> 12) - t43a;
    t53a = ((t42 * (4096 - 3784) + t53 * 1567 + 2048) >> 12) - t42;
    t54 = ((t41a * (4096 - 3784) + t54a * 1567 + 2048) >> 12) - t41a;
    t55a = ((t40 * (4096 - 3784) + t55 * 1567 + 2048) >> 12) - t40;
    t56a = ((t39 * 1567 + t56 * (3784 - 4096) + 2048) >> 12) + t56;
    t57 = ((t38a * 1567 + t57a * (3784 - 4096) + 2048) >> 12) + t57a;
    t58a = ((t37 * 1567 + t58 * (3784 - 4096) + 2048) >> 12) + t58;
    t59 = ((t36a * 1567 + t59a * (3784 - 4096) + 2048) >> 12) + t59a;

    t32a = clip(t32 + t47, min, max);
    t33 = clip(t33a + t46a, min, max);
    t34a = clip(t34 + t45, min, max);
    t35 = clip(t35a + t44a, min, max);
    t36a = clip(t36 + t43, min, max);
    t37 = clip(t37a + t42a, min, max);
    t38a = clip(t38 + t41, min, max);
    t39 = clip(t39a + t40a, min, max);
    t40 = clip(t39a - t40a, min, max);
    t41a = clip(t38 - t41, min, max);
    t42 = clip(t37a - t42a, min, max);
    t43a = clip(t36 - t43, min, max);
    t44 = clip(t35a - t44a, min, max);
    t45a = clip(t34 - t45, min, max);
    t46 = clip(t33a - t46a, min, max);
    t47a = clip(t32 - t47, min, max);
    t48a = clip(t63 - t48, min, max);
    t49 = clip(t62a - t49a, min, max);
    t50a = clip(t61 - t50, min, max);
    t51 = clip(t60a - t51a, min, max);
    t52a = clip(t59 - t52, min, max);
    t53 = clip(t58a - t53a, min, max);
    t54a = clip(t57 - t54, min, max);
    t55 = clip(t56a - t55a, min, max);
    t56 = clip(t56a + t55a, min, max);
    t57a = clip(t57 + t54, min, max);
    t58 = clip(t58a + t53a, min, max);
    t59a = clip(t59 + t52, min, max);
    t60 = clip(t60a + t51a, min, max);
    t61a = clip(t61 + t50, min, max);
    t62 = clip(t62a + t49a, min, max);
    t63a = clip(t63 + t48, min, max);

    t40a = ((t55 - t40) * 181 + 128) >> 8;
    t41 = ((t54a - t41a) * 181 + 128) >> 8;
    t42a = ((t53 - t42) * 181 + 128) >> 8;
    t43 = ((t52a - t43a) * 181 + 128) >> 8;
    t44a = ((t51 - t44) * 181 + 128) >> 8;
    t45 = ((t50a - t45a) * 181 + 128) >> 8;
    t46a = ((t49 - t46) * 181 + 128) >> 8;
    t47 = ((t48a - t47a) * 181 + 128) >> 8;
    t48 = ((t47a + t48a) * 181 + 128) >> 8;
    t49a = ((t46 + t49) * 181 + 128) >> 8;
    t50 = ((t45a + t50a) * 181 + 128) >> 8;
    t51a = ((t44 + t51) * 181 + 128) >> 8;
    t52 = ((t43a + t52a) * 181 + 128) >> 8;
    t53a = ((t42 + t53) * 181 + 128) >> 8;
    t54 = ((t41a + t54a) * 181 + 128) >> 8;
    t55a = ((t40 + t55) * 181 + 128) >> 8;

    let tt = [
        c[0], c[2 * s], c[4 * s], c[6 * s], c[8 * s], c[10 * s], c[12 * s], c[14 * s],
        c[16 * s], c[18 * s], c[20 * s], c[22 * s], c[24 * s], c[26 * s], c[28 * s], c[30 * s],
        c[32 * s], c[34 * s], c[36 * s], c[38 * s], c[40 * s], c[42 * s], c[44 * s], c[46 * s],
        c[48 * s], c[50 * s], c[52 * s], c[54 * s], c[56 * s], c[58 * s], c[60 * s], c[62 * s],
    ];
    let hi = [
        t63a, t62, t61a, t60, t59a, t58, t57a, t56, t55a, t54, t53a, t52, t51a, t50, t49a, t48,
        t47, t46a, t45, t44a, t43, t42a, t41, t40a, t39, t38a, t37, t36a, t35, t34a, t33, t32a,
    ];
    for i in 0..32 {
        c[i * s] = clip(tt[i] + hi[i], min, max);
        c[(63 - i) * s] = clip(tt[i] - hi[i], min, max);
    }
}

// ---- Inverse ADST / FlipADST --------------------------------------------
// dav1d's `inv_adstN_1d_internal_c` reads `in` forward and writes `out`; the
// flip variant writes the output reversed (negative out stride). We compute the
// outputs into an array then store forward (ADST) or reversed (FlipADST).

#[inline]
fn store_adst(c: &mut [i32], s: usize, out: &[i32], flip: bool) {
    let n = out.len();
    for (i, &v) in out.iter().enumerate() {
        let idx = if flip { n - 1 - i } else { i };
        c[idx * s] = v;
    }
}

fn adst4(c: &mut [i32], s: usize, _min: i32, _max: i32, flip: bool) {
    let (in0, in1, in2, in3) = (c[0], c[s], c[2 * s], c[3 * s]);
    let o0 = ((1321 * in0 + (3803 - 4096) * in2 + (2482 - 4096) * in3 + (3344 - 4096) * in1 + 2048)
        >> 12)
        + in2
        + in3
        + in1;
    let o1 = (((2482 - 4096) * in0 - 1321 * in2 - (3803 - 4096) * in3 + (3344 - 4096) * in1 + 2048)
        >> 12)
        + in0
        - in3
        + in1;
    let o2 = (209 * (in0 - in2 + in3) + 128) >> 8;
    let o3 = (((3803 - 4096) * in0 + (2482 - 4096) * in2 - 1321 * in3 - (3344 - 4096) * in1 + 2048)
        >> 12)
        + in0
        + in2
        - in1;
    store_adst(c, s, &[o0, o1, o2, o3], flip);
}

fn adst8(c: &mut [i32], s: usize, min: i32, max: i32, flip: bool) {
    let (in0, in1, in2, in3) = (c[0], c[s], c[2 * s], c[3 * s]);
    let (in4, in5, in6, in7) = (c[4 * s], c[5 * s], c[6 * s], c[7 * s]);
    let t0a = (((4076 - 4096) * in7 + 401 * in0 + 2048) >> 12) + in7;
    let t1a = ((401 * in7 - (4076 - 4096) * in0 + 2048) >> 12) - in0;
    let t2a = (((3612 - 4096) * in5 + 1931 * in2 + 2048) >> 12) + in5;
    let t3a = ((1931 * in5 - (3612 - 4096) * in2 + 2048) >> 12) - in2;
    let t4a = (1299 * in3 + 1583 * in4 + 1024) >> 11;
    let t5a = (1583 * in3 - 1299 * in4 + 1024) >> 11;
    let t6a = ((1189 * in1 + (3920 - 4096) * in6 + 2048) >> 12) + in6;
    let t7a = (((3920 - 4096) * in1 - 1189 * in6 + 2048) >> 12) + in1;
    let t0 = clip(t0a + t4a, min, max);
    let t1 = clip(t1a + t5a, min, max);
    let mut t2 = clip(t2a + t6a, min, max);
    let mut t3 = clip(t3a + t7a, min, max);
    let t4 = clip(t0a - t4a, min, max);
    let t5 = clip(t1a - t5a, min, max);
    let mut t6 = clip(t2a - t6a, min, max);
    let mut t7 = clip(t3a - t7a, min, max);
    let t4a = (((3784 - 4096) * t4 + 1567 * t5 + 2048) >> 12) + t4;
    let t5a = ((1567 * t4 - (3784 - 4096) * t5 + 2048) >> 12) - t5;
    let t6a = (((3784 - 4096) * t7 - 1567 * t6 + 2048) >> 12) + t7;
    let t7a = ((1567 * t7 + (3784 - 4096) * t6 + 2048) >> 12) + t6;
    let mut out = [0i32; 8];
    out[0] = clip(t0 + t2, min, max);
    out[7] = -clip(t1 + t3, min, max);
    t2 = clip(t0 - t2, min, max);
    t3 = clip(t1 - t3, min, max);
    out[1] = -clip(t4a + t6a, min, max);
    out[6] = clip(t5a + t7a, min, max);
    t6 = clip(t4a - t6a, min, max);
    t7 = clip(t5a - t7a, min, max);
    out[3] = -(((t2 + t3) * 181 + 128) >> 8);
    out[4] = ((t2 - t3) * 181 + 128) >> 8;
    out[2] = ((t6 + t7) * 181 + 128) >> 8;
    out[5] = -(((t6 - t7) * 181 + 128) >> 8);
    store_adst(c, s, &out, flip);
}

fn adst16(c: &mut [i32], s: usize, min: i32, max: i32, flip: bool) {
    let r = |i: usize| c[i * s];
    let (in0, in1, in2, in3) = (r(0), r(1), r(2), r(3));
    let (in4, in5, in6, in7) = (r(4), r(5), r(6), r(7));
    let (in8, in9, in10, in11) = (r(8), r(9), r(10), r(11));
    let (in12, in13, in14, in15) = (r(12), r(13), r(14), r(15));
    let mut t0 = ((in15 * (4091 - 4096) + in0 * 201 + 2048) >> 12) + in15;
    let mut t1 = ((in15 * 201 - in0 * (4091 - 4096) + 2048) >> 12) - in0;
    let mut t2 = ((in13 * (3973 - 4096) + in2 * 995 + 2048) >> 12) + in13;
    let mut t3 = ((in13 * 995 - in2 * (3973 - 4096) + 2048) >> 12) - in2;
    let mut t4 = ((in11 * (3703 - 4096) + in4 * 1751 + 2048) >> 12) + in11;
    let mut t5 = ((in11 * 1751 - in4 * (3703 - 4096) + 2048) >> 12) - in4;
    let mut t6 = (in9 * 1645 + in6 * 1220 + 1024) >> 11;
    let mut t7 = (in9 * 1220 - in6 * 1645 + 1024) >> 11;
    let mut t8 = ((in7 * 2751 + in8 * (3035 - 4096) + 2048) >> 12) + in8;
    let mut t9 = ((in7 * (3035 - 4096) - in8 * 2751 + 2048) >> 12) + in7;
    let mut t10 = ((in5 * 2106 + in10 * (3513 - 4096) + 2048) >> 12) + in10;
    let mut t11 = ((in5 * (3513 - 4096) - in10 * 2106 + 2048) >> 12) + in5;
    let mut t12 = ((in3 * 1380 + in12 * (3857 - 4096) + 2048) >> 12) + in12;
    let mut t13 = ((in3 * (3857 - 4096) - in12 * 1380 + 2048) >> 12) + in3;
    let mut t14 = ((in1 * 601 + in14 * (4052 - 4096) + 2048) >> 12) + in14;
    let mut t15 = ((in1 * (4052 - 4096) - in14 * 601 + 2048) >> 12) + in1;
    let t0a = clip(t0 + t8, min, max);
    let t1a = clip(t1 + t9, min, max);
    let mut t2a = clip(t2 + t10, min, max);
    let mut t3a = clip(t3 + t11, min, max);
    let mut t4a = clip(t4 + t12, min, max);
    let mut t5a = clip(t5 + t13, min, max);
    let mut t6a = clip(t6 + t14, min, max);
    let mut t7a = clip(t7 + t15, min, max);
    let mut t8a = clip(t0 - t8, min, max);
    let mut t9a = clip(t1 - t9, min, max);
    let mut t10a = clip(t2 - t10, min, max);
    let mut t11a = clip(t3 - t11, min, max);
    let mut t12a = clip(t4 - t12, min, max);
    let mut t13a = clip(t5 - t13, min, max);
    let mut t14a = clip(t6 - t14, min, max);
    let mut t15a = clip(t7 - t15, min, max);
    t8 = ((t8a * (4017 - 4096) + t9a * 799 + 2048) >> 12) + t8a;
    t9 = ((t8a * 799 - t9a * (4017 - 4096) + 2048) >> 12) - t9a;
    t10 = ((t10a * 2276 + t11a * (3406 - 4096) + 2048) >> 12) + t11a;
    t11 = ((t10a * (3406 - 4096) - t11a * 2276 + 2048) >> 12) + t10a;
    t12 = ((t13a * (4017 - 4096) - t12a * 799 + 2048) >> 12) + t13a;
    t13 = ((t13a * 799 + t12a * (4017 - 4096) + 2048) >> 12) + t12a;
    t14 = ((t15a * 2276 - t14a * (3406 - 4096) + 2048) >> 12) - t14a;
    t15 = ((t15a * (3406 - 4096) + t14a * 2276 + 2048) >> 12) + t15a;
    t0 = clip(t0a + t4a, min, max);
    t1 = clip(t1a + t5a, min, max);
    t2 = clip(t2a + t6a, min, max);
    t3 = clip(t3a + t7a, min, max);
    t4 = clip(t0a - t4a, min, max);
    t5 = clip(t1a - t5a, min, max);
    t6 = clip(t2a - t6a, min, max);
    t7 = clip(t3a - t7a, min, max);
    t8a = clip(t8 + t12, min, max);
    t9a = clip(t9 + t13, min, max);
    t10a = clip(t10 + t14, min, max);
    t11a = clip(t11 + t15, min, max);
    t12a = clip(t8 - t12, min, max);
    t13a = clip(t9 - t13, min, max);
    t14a = clip(t10 - t14, min, max);
    t15a = clip(t11 - t15, min, max);
    t4a = ((t4 * (3784 - 4096) + t5 * 1567 + 2048) >> 12) + t4;
    t5a = ((t4 * 1567 - t5 * (3784 - 4096) + 2048) >> 12) - t5;
    t6a = ((t7 * (3784 - 4096) - t6 * 1567 + 2048) >> 12) + t7;
    t7a = ((t7 * 1567 + t6 * (3784 - 4096) + 2048) >> 12) + t6;
    t12 = ((t12a * (3784 - 4096) + t13a * 1567 + 2048) >> 12) + t12a;
    t13 = ((t12a * 1567 - t13a * (3784 - 4096) + 2048) >> 12) - t13a;
    t14 = ((t15a * (3784 - 4096) - t14a * 1567 + 2048) >> 12) + t15a;
    t15 = ((t15a * 1567 + t14a * (3784 - 4096) + 2048) >> 12) + t14a;
    let mut out = [0i32; 16];
    out[0] = clip(t0 + t2, min, max);
    out[15] = -clip(t1 + t3, min, max);
    t2a = clip(t0 - t2, min, max);
    t3a = clip(t1 - t3, min, max);
    out[3] = -clip(t4a + t6a, min, max);
    out[12] = clip(t5a + t7a, min, max);
    t6 = clip(t4a - t6a, min, max);
    t7 = clip(t5a - t7a, min, max);
    out[1] = -clip(t8a + t10a, min, max);
    out[14] = clip(t9a + t11a, min, max);
    t10 = clip(t8a - t10a, min, max);
    t11 = clip(t9a - t11a, min, max);
    out[2] = clip(t12 + t14, min, max);
    out[13] = -clip(t13 + t15, min, max);
    t14a = clip(t12 - t14, min, max);
    t15a = clip(t13 - t15, min, max);
    out[7] = -(((t2a + t3a) * 181 + 128) >> 8);
    out[8] = ((t2a - t3a) * 181 + 128) >> 8;
    out[4] = ((t6 + t7) * 181 + 128) >> 8;
    out[11] = -(((t6 - t7) * 181 + 128) >> 8);
    out[6] = ((t10 + t11) * 181 + 128) >> 8;
    out[9] = -(((t10 - t11) * 181 + 128) >> 8);
    out[5] = -(((t14a + t15a) * 181 + 128) >> 8);
    out[10] = ((t14a - t15a) * 181 + 128) >> 8;
    store_adst(c, s, &out, flip);
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
            0 => dct4(c, s, min, max, false),
            1 => dct8(c, s, min, max, false),
            2 => dct16(c, s, min, max, false),
            3 => dct32(c, s, min, max, false),
            _ => dct64(c, s, min, max),
        },
        IDENTITY_1D => match len_log2 {
            0 => identity4(c, s, min, max),
            1 => identity8(c, s, min, max),
            2 => identity16(c, s, min, max),
            _ => identity32(c, s, min, max),
        },
        ADST_1D => match len_log2 {
            0 => adst4(c, s, min, max, false),
            1 => adst8(c, s, min, max, false),
            _ => adst16(c, s, min, max, false),
        },
        FLIPADST_1D => match len_log2 {
            0 => adst4(c, s, min, max, true),
            1 => adst8(c, s, min, max, true),
            _ => adst16(c, s, min, max, true),
        },
        // ADST/FlipADST only exist for 4/8/16; DCT covers 32/64.
        _ => unreachable!("invalid 1D transform type {ty}"),
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
    // The buffer spans the full `h` rows so the 64-point column pass (which reads
    // all `h` rows, with rows ≥ sh implicitly zero) stays in bounds.
    let mut tmp = vec![0i32; w * h];
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
        // (n, ty, len_log2, ref_index). ITX_REF order: dct4/8/16/32, adst4/8/16,
        // flipadst4/8/16, idt4/8/16/32, wht4, dct64.
        let cases: [(usize, u8, usize, usize); 16] = [
            (4, DCT_1D, 0, 0),
            (8, DCT_1D, 1, 1),
            (16, DCT_1D, 2, 2),
            (32, DCT_1D, 3, 3),
            (64, DCT_1D, 4, 15),
            (4, ADST_1D, 0, 4),
            (8, ADST_1D, 1, 5),
            (16, ADST_1D, 2, 6),
            (4, FLIPADST_1D, 0, 7),
            (8, FLIPADST_1D, 1, 8),
            (16, FLIPADST_1D, 2, 9),
            (4, IDENTITY_1D, 0, 10),
            (8, IDENTITY_1D, 1, 11),
            (16, IDENTITY_1D, 2, 12),
            (32, IDENTITY_1D, 3, 13),
            (4, 255, 0, 14), // WHT sentinel
        ];
        for &(n, ty, ll, ri) in cases.iter() {
            let mut c = input(n);
            if ty == 255 {
                wht4(&mut c, 1);
            } else {
                itx_1d(&mut c, 1, ll, ty, mn, mx);
            }
            assert_eq!(c, ITX_REF[ri], "1D transform (n={n}, ty={ty}, ref={ri})");
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
        let mut cf: Vec<i32> = (0..64).map(|i| ((i * 11) % 23) - 11).collect();
        let res = inv_txfm_residual(&mut cf, 1, txtp::DCT_DCT, 20);
        assert_eq!(res.len(), 64);
        assert!(res.iter().all(|&v| v.abs() < 1 << 20));
    }

    #[test]
    fn inv_txfm_2d_64x64_runs_without_oob() {
        // TX_64X64 (tx index 4): the column pass runs a 64-point DCT over the full
        // 64 rows even though only the top-left 32×32 coefficients are non-zero —
        // exercises the `w*h` intermediate buffer (no OOB) + the dct64 dispatch.
        let mut cf = vec![0i32; 32 * 32];
        for (i, v) in cf.iter_mut().enumerate() {
            *v = ((i as i32 * 7) % 29) - 14;
        }
        let res = inv_txfm_residual(&mut cf, 4, txtp::DCT_DCT, 200);
        assert_eq!(res.len(), 64 * 64);
        assert!(res.iter().all(|&v| v.abs() < 1 << 20));
    }
}
