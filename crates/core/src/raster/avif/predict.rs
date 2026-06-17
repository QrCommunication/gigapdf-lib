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

fn pred_dc(out: &mut [i32], bw: usize, bh: usize, top: &[i32], left: &[i32], ht: bool, hl: bool) {
    let dc: i32 = match (ht, hl) {
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
    };
    out.fill(dc);
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
        for (si, &n) in [4usize, 8].iter().enumerate() {
            let (top, left, tl) = edges(n);
            let base = si * 7;
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
        }
    }
}
