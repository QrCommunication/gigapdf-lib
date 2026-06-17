//! AV1 intra predictors (DC, V, H, Paeth, Smooth family).
//!
//! Faithful translation of dav1d `src/ipred_tmpl.c` (BSD-2-Clause). Each
//! predictor fills a `bw*bh` row-major block from the top/left/top-left edge
//! samples the caller assembled (with availability fills per
//! `dav1d_prepare_intra_edges`). DC and Paeth apply the `av1_mode_conv`
//! availability downgrade. Directional (Z1/Z2/Z3), CfL and filter-intra land in
//! a follow-up — they fall back to DC here.

#![allow(dead_code)]

pub(super) const DC_PRED: u8 = 0;
pub(super) const VERT_PRED: u8 = 1;
pub(super) const HOR_PRED: u8 = 2;
pub(super) const SMOOTH_PRED: u8 = 9;
pub(super) const SMOOTH_V_PRED: u8 = 10;
pub(super) const SMOOTH_H_PRED: u8 = 11;
pub(super) const PAETH_PRED: u8 = 12;
pub(super) const CFL_PRED: u8 = 13;
pub(super) const FILTER_PRED: u8 = 13;
/// Internal sentinel for the DC_128 downgrade (flat mid-grey).
const DC_128: u8 = 255;

/// `dav1d_sm_weights[128]` — SMOOTH blend weights, indexed `[size + i]`
/// (size = block dim, ≥2). From dav1d `src/tables.c` (BSD-2-Clause).
pub(super) static SM_WEIGHTS: [i32; 128] = [
    0, 0, 255, 128, 255, 149, 85, 64, 255, 197, 146, 105, 73, 50, 37, 32,
    255, 225, 196, 170, 145, 123, 102, 84, 68, 54, 43, 33, 26, 20, 17, 16,
    255, 240, 225, 210, 196, 182, 169, 157, 145, 133, 122, 111, 101, 92, 83, 74,
    66, 59, 52, 45, 39, 34, 29, 25, 21, 17, 14, 12, 10, 9, 8, 8,
    255, 248, 240, 233, 225, 218, 210, 203, 196, 189, 182, 176, 169, 163, 156, 150,
    144, 138, 133, 127, 121, 116, 111, 106, 101, 96, 91, 86, 82, 77, 73, 69,
    65, 61, 57, 54, 50, 47, 44, 41, 38, 35, 32, 29, 27, 25, 22, 20,
    18, 16, 15, 13, 12, 10, 9, 8, 7, 6, 6, 5, 5, 4, 4, 4,
];

/// The scalar DC prediction value (dav1d `dc_gen`/`dc_gen_top`/`dc_gen_left`),
/// also the base for CfL. 128 when neither neighbour is available.
pub(super) fn dc_value(bw: usize, bh: usize, top: &[i32], left: &[i32], ht: bool, hl: bool) -> i32 {
    match (ht, hl) {
        (true, true) => {
            let mut s = ((bw + bh) >> 1) as i32;
            for &t in top.iter().take(bw) {
                s += t;
            }
            for &l in left.iter().take(bh) {
                s += l;
            }
            s >>= (bw + bh).trailing_zeros();
            if bw != bh {
                let m = if bw > bh * 2 || bh > bw * 2 { 0x3334 } else { 0x5556 };
                s = (s * m) >> 16;
            }
            s
        }
        (true, false) => {
            let mut s = (bw >> 1) as i32;
            for &t in top.iter().take(bw) {
                s += t;
            }
            s >> bw.trailing_zeros()
        }
        (false, true) => {
            let mut s = (bh >> 1) as i32;
            for &l in left.iter().take(bh) {
                s += l;
            }
            s >> bh.trailing_zeros()
        }
        (false, false) => 128,
    }
}

fn pred_dc(out: &mut [i32], bw: usize, bh: usize, top: &[i32], left: &[i32], ht: bool, hl: bool) {
    out.fill(dc_value(bw, bh, top, left, ht, hl));
}

fn pred_v(out: &mut [i32], bw: usize, bh: usize, top: &[i32]) {
    for y in 0..bh {
        out[y * bw..y * bw + bw].copy_from_slice(&top[..bw]);
    }
}

fn pred_h(out: &mut [i32], bw: usize, bh: usize, left: &[i32]) {
    for y in 0..bh {
        out[y * bw..y * bw + bw].fill(left[y]);
    }
}

fn pred_paeth(out: &mut [i32], bw: usize, bh: usize, top: &[i32], left: &[i32], tl: i32) {
    for y in 0..bh {
        let l = left[y];
        for x in 0..bw {
            let t = top[x];
            let base = l + t - tl;
            let ld = (l - base).abs();
            let td = (t - base).abs();
            let tld = (tl - base).abs();
            out[y * bw + x] = if ld <= td && ld <= tld {
                l
            } else if td <= tld {
                t
            } else {
                tl
            };
        }
    }
}

fn pred_smooth(out: &mut [i32], bw: usize, bh: usize, top: &[i32], left: &[i32]) {
    let wv = &SM_WEIGHTS[bh..];
    let wh = &SM_WEIGHTS[bw..];
    let right = top[bw - 1];
    let bottom = left[bh - 1];
    for y in 0..bh {
        for x in 0..bw {
            let pred = wv[y] * top[x]
                + (256 - wv[y]) * bottom
                + wh[x] * left[y]
                + (256 - wh[x]) * right;
            out[y * bw + x] = (pred + 256) >> 9;
        }
    }
}

fn pred_smooth_v(out: &mut [i32], bw: usize, bh: usize, top: &[i32], left: &[i32]) {
    let wv = &SM_WEIGHTS[bh..];
    let bottom = left[bh - 1];
    for y in 0..bh {
        for x in 0..bw {
            let pred = wv[y] * top[x] + (256 - wv[y]) * bottom;
            out[y * bw + x] = (pred + 128) >> 8;
        }
    }
}

fn pred_smooth_h(out: &mut [i32], bw: usize, bh: usize, top: &[i32], left: &[i32]) {
    let wh = &SM_WEIGHTS[bw..];
    let right = top[bw - 1];
    for y in 0..bh {
        for x in 0..bw {
            let pred = wh[x] * left[y] + (256 - wh[x]) * right;
            out[y * bw + x] = (pred + 128) >> 8;
        }
    }
}

/// `dav1d_filter_intra_taps[5][8][7]` — filter-intra weights: [mode][output
/// (yy*4+xx, 0..7)][tap p0..p6]. From dav1d `src/tables.c` (BSD-2-Clause).
pub(super) static FILTER_INTRA_TAPS: [[[i32; 7]; 8]; 5] = [
    [
        [-6, 10, 0, 0, 0, 12, 0],
        [-5, 2, 10, 0, 0, 9, 0],
        [-3, 1, 1, 10, 0, 7, 0],
        [-3, 1, 1, 2, 10, 5, 0],
        [-4, 6, 0, 0, 0, 2, 12],
        [-3, 2, 6, 0, 0, 2, 9],
        [-3, 2, 2, 6, 0, 2, 7],
        [-3, 1, 2, 2, 6, 3, 5],
    ],
    [
        [-10, 16, 0, 0, 0, 10, 0],
        [-6, 0, 16, 0, 0, 6, 0],
        [-4, 0, 0, 16, 0, 4, 0],
        [-2, 0, 0, 0, 16, 2, 0],
        [-10, 16, 0, 0, 0, 0, 10],
        [-6, 0, 16, 0, 0, 0, 6],
        [-4, 0, 0, 16, 0, 0, 4],
        [-2, 0, 0, 0, 16, 0, 2],
    ],
    [
        [-8, 8, 0, 0, 0, 16, 0],
        [-8, 0, 8, 0, 0, 16, 0],
        [-8, 0, 0, 8, 0, 16, 0],
        [-8, 0, 0, 0, 8, 16, 0],
        [-4, 4, 0, 0, 0, 0, 16],
        [-4, 0, 4, 0, 0, 0, 16],
        [-4, 0, 0, 4, 0, 0, 16],
        [-4, 0, 0, 0, 4, 0, 16],
    ],
    [
        [-2, 8, 0, 0, 0, 10, 0],
        [-1, 3, 8, 0, 0, 6, 0],
        [-1, 2, 3, 8, 0, 4, 0],
        [0, 1, 2, 3, 8, 2, 0],
        [-1, 4, 0, 0, 0, 3, 10],
        [-1, 3, 4, 0, 0, 4, 6],
        [-1, 2, 3, 4, 0, 4, 4],
        [-1, 2, 2, 3, 4, 3, 3],
    ],
    [
        [-12, 14, 0, 0, 0, 14, 0],
        [-10, 0, 14, 0, 0, 12, 0],
        [-9, 0, 0, 14, 0, 11, 0],
        [-8, 0, 0, 0, 14, 10, 0],
        [-10, 12, 0, 0, 0, 0, 14],
        [-9, 1, 12, 0, 0, 0, 12],
        [-8, 0, 0, 12, 0, 1, 11],
        [-7, 0, 0, 1, 12, 1, 9],
    ],
];

/// Filter-intra prediction (`ipred_filter_c`): recursive 4×2 blocks, each output
/// a 7-tap weighted sum of the top-left/top/left references, where later blocks
/// read the already-written outputs of earlier ones. `filt_idx` selects the mode.
pub(super) fn filter(
    bw: usize,
    bh: usize,
    top: &[i32],
    left: &[i32],
    topleft: i32,
    filt_idx: usize,
) -> Vec<i32> {
    let taps = &FILTER_INTRA_TAPS[filt_idx.min(4)];
    let mut out = vec![0i32; bw * bh];
    let mut y = 0;
    while y < bh {
        let mut x = 0;
        while x < bw {
            // 7 references for this 4×2 block (dav1d pointer semantics).
            let (p1, p2, p3, p4) = if y == 0 {
                (top[x], top[x + 1], top[x + 2], top[x + 3])
            } else {
                let r = (y - 1) * bw + x;
                (out[r], out[r + 1], out[r + 2], out[r + 3])
            };
            let p0 = if x == 0 {
                if y == 0 { topleft } else { left[y - 1] }
            } else if y == 0 {
                top[x - 1]
            } else {
                out[(y - 1) * bw + x - 1]
            };
            let (p5, p6) = if x == 0 {
                (left[y], left[y + 1])
            } else {
                (out[y * bw + x - 1], out[(y + 1) * bw + x - 1])
            };
            for yy in 0..2 {
                for xx in 0..4 {
                    let t = &taps[yy * 4 + xx];
                    let acc = t[0] * p0
                        + t[1] * p1
                        + t[2] * p2
                        + t[3] * p3
                        + t[4] * p4
                        + t[5] * p5
                        + t[6] * p6;
                    out[(y + yy) * bw + x + xx] = ((acc + 8) >> 4).clamp(0, 255);
                }
            }
            x += 4;
        }
        y += 2;
    }
    out
}

/// Chroma-from-luma final blend (`cfl_pred`): `chroma = clip(dc + sign(d) *
/// ((|d| + 32) >> 6))` where `d = alpha * ac`. `ac` is the mean-removed,
/// subsampled luma AC; `dc` the chroma DC prediction; `alpha` the signed CfL gain.
pub(super) fn cfl_apply(dc: i32, ac: &[i32], alpha: i32) -> Vec<i32> {
    ac.iter()
        .map(|&a| {
            let d = alpha * a;
            let v = (d.abs() + 32) >> 6;
            (dc + if d < 0 { -v } else { v }).clamp(0, 255)
        })
        .collect()
}

/// Predict a `bw*bh` block for intra `mode` from assembled edges. `top`/`left`
/// hold ≥ `bw`/`bh` samples; `topleft` is the corner. Returns the row-major
/// predicted block.
#[allow(clippy::too_many_arguments)]
pub(super) fn predict(
    mode: u8,
    have_top: bool,
    have_left: bool,
    bw: usize,
    bh: usize,
    top: &[i32],
    left: &[i32],
    topleft: i32,
) -> Vec<i32> {
    let mut out = vec![0i32; bw * bh];
    // av1_mode_conv availability downgrade for DC + Paeth.
    let m = match mode {
        PAETH_PRED => match (have_left, have_top) {
            (false, false) => DC_128,
            (false, true) => VERT_PRED,
            (true, false) => HOR_PRED,
            (true, true) => PAETH_PRED,
        },
        other => other,
    };
    match m {
        VERT_PRED => pred_v(&mut out, bw, bh, top),
        HOR_PRED => pred_h(&mut out, bw, bh, left),
        PAETH_PRED => pred_paeth(&mut out, bw, bh, top, left, topleft),
        SMOOTH_PRED => pred_smooth(&mut out, bw, bh, top, left),
        SMOOTH_V_PRED => pred_smooth_v(&mut out, bw, bh, top, left),
        SMOOTH_H_PRED => pred_smooth_h(&mut out, bw, bh, top, left),
        DC_128 => out.fill(128),
        // DC_PRED and not-yet-implemented modes (directional/CfL/filter) use DC.
        _ => pred_dc(&mut out, bw, bh, top, left, have_top, have_left),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("ipred_ref.rs");

    fn edges(n: usize) -> (Vec<i32>, Vec<i32>, i32) {
        let top = (0..2 * n).map(|i| 130 + 3 * i as i32).collect();
        let left = (0..2 * n).map(|i| 110 - 2 * i as i32).collect();
        (top, left, 100)
    }

    #[test]
    fn predictors_match_dav1d() {
        // IPRED_REF order per size (stride 12): dc, v, h, paeth, smooth,
        // smooth_v, smooth_h, filter0..filter4.
        for (si, &n) in [4usize, 8].iter().enumerate() {
            let (top, left, tl) = edges(n);
            let base = si * 12;
            let modes = [
                DC_PRED,
                VERT_PRED,
                HOR_PRED,
                PAETH_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
            ];
            for (j, &mode) in modes.iter().enumerate() {
                let out = predict(mode, true, true, n, n, &top, &left, tl);
                assert_eq!(out, IPRED_REF[base + j], "mode {mode} size {n}");
            }
            for fi in 0..5 {
                let out = filter(n, n, &top, &left, tl, fi);
                assert_eq!(out, IPRED_REF[base + 7 + fi], "filter {fi} size {n}");
            }
        }
    }

    #[test]
    fn cfl_apply_matches_formula() {
        // chroma = clip(dc + sign(d)*((|d|+32)>>6)), d = alpha*ac.
        assert_eq!(cfl_apply(128, &[64, -64, 0], 0), vec![128, 128, 128]); // alpha 0
        assert_eq!(cfl_apply(128, &[64], 2), vec![130]); // d=128 → +2
        assert_eq!(cfl_apply(128, &[-64], 2), vec![126]); // d=-128 → -2
        assert_eq!(cfl_apply(10, &[-1000], 5), vec![0]); // clamps at 0
        assert_eq!(cfl_apply(250, &[1000], 5), vec![255]); // clamps at 255
    }
}
