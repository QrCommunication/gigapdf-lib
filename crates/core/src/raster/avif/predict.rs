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


// ---- Directional (Z1/Z2/Z3) edge helpers (dav1d ipred_tmpl.c) -------------

/// `dav1d_dr_intra_derivative[44]` — directional intra slope per (angle>>1).
/// Zero entries are unused. From dav1d `src/tables.c` (BSD-2-Clause).
pub(super) static DR_INTRA_DERIVATIVE: [i32; 44] = [
    0, 1023, 0, 547, 372, 0, 0, 273, 215, 0, 178,
    151, 0, 132, 116, 0, 102, 0, 90, 80, 0, 71,
    64, 0, 57, 51, 0, 45, 0, 40, 35, 0, 31,
    27, 0, 23, 19, 0, 15, 0, 11, 0, 7, 3,
];
/// `get_filter_strength`: intra-edge low-pass strength (0=off..3) by block size,
/// angle-from-orthogonal and the smooth-neighbour flag.
pub(super) fn get_filter_strength(wh: i32, angle: i32, is_sm: bool) -> i32 {
    if is_sm {
        if wh <= 8 {
            if angle >= 64 { return 2; }
            if angle >= 40 { return 1; }
        } else if wh <= 16 {
            if angle >= 48 { return 2; }
            if angle >= 20 { return 1; }
        } else if wh <= 24 {
            if angle >= 4 { return 3; }
        } else {
            return 3;
        }
    } else if wh <= 8 {
        if angle >= 56 { return 1; }
    } else if wh <= 16 {
        if angle >= 40 { return 1; }
    } else if wh <= 24 {
        if angle >= 32 { return 3; }
        if angle >= 16 { return 2; }
        if angle >= 8 { return 1; }
    } else if wh <= 32 {
        if angle >= 32 { return 3; }
        if angle >= 4 { return 2; }
        return 1;
    } else {
        return 3;
    }
    0
}

/// `get_upsample`: whether to 2× upsample the reference edge.
pub(super) fn get_upsample(wh: i32, angle: i32, is_sm: bool) -> bool {
    angle < 40 && wh <= (16 >> is_sm as i32)
}

/// `filter_edge`: 5-tap low-pass over `inp[from..to]` (clamped), writing `sz`
/// samples to `out`. `inp_at(k)` = dav1d `in[iclip(k, from, to-1)]`.
pub(super) fn filter_edge(
    out: &mut [i32],
    sz: usize,
    lim_from: usize,
    lim_to: usize,
    inp: &dyn Fn(i32) -> i32,
    strength: usize,
) {
    const KERNEL: [[i32; 5]; 3] = [[0, 4, 8, 4, 0], [0, 5, 6, 5, 0], [2, 4, 4, 4, 2]];
    let mut i = 0usize;
    while i < sz.min(lim_from) {
        out[i] = inp(i as i32);
        i += 1;
    }
    while i < lim_to.min(sz) {
        let mut s = 0i32;
        for (j, &k) in KERNEL[strength - 1].iter().enumerate() {
            s += inp(i as i32 - 2 + j as i32) * k;
        }
        out[i] = (s + 8) >> 4;
        i += 1;
    }
    while i < sz {
        out[i] = inp(i as i32);
        i += 1;
    }
}

/// `upsample_edge`: 2× upsample `hsz` samples with a [-1,9,9,-1] interpolation.
pub(super) fn upsample_edge(out: &mut [i32], hsz: usize, inp: &dyn Fn(i32) -> i32) {
    const KERNEL: [i32; 4] = [-1, 9, 9, -1];
    let mut i = 0usize;
    while i < hsz - 1 {
        out[i * 2] = inp(i as i32);
        let mut s = 0i32;
        for (j, &k) in KERNEL.iter().enumerate() {
            s += inp(i as i32 + j as i32 - 1) * k;
        }
        out[i * 2 + 1] = ((s + 8) >> 4).clamp(0, 255);
        i += 1;
    }
    out[i * 2] = inp(i as i32);
}

/// Directional Z1 predictor (angle < 90, uses the top edge). `tl` is the edge
/// buffer with the corner at index `corner`, top samples at `corner+1+i` (incl.
/// the top-right extension). `angle_full` carries the angle (bits 0-8) plus the
/// `is_sm` (bit 9) and `enable_intra_edge_filter` (bit 10) flags. dav1d `ipred_z1_c`.
pub(super) fn z1(bw: usize, bh: usize, angle_full: i32, tl: &[i32], corner: usize) -> Vec<i32> {
    let is_sm = (angle_full >> 9) & 1 != 0;
    let enable = angle_full >> 10 != 0;
    let angle = angle_full & 511;
    let mut dx = DR_INTRA_DERIVATIVE[(angle >> 1) as usize];
    let wh = (bw + bh) as i32;
    let from = -1i32;
    let to = (bw + bw.min(bh)) as i32;
    let inn = |k: i32| -> i32 {
        let c = k.clamp(from, to - 1);
        tl[(corner as i32 + 1 + c) as usize]
    };
    let upsample = enable && get_upsample(wh, 90 - angle, is_sm);
    let mut buf = vec![0i32; 2 * (bw + bh) + 16];
    let top: Vec<i32>;
    let max_base_x;
    if upsample {
        upsample_edge(&mut buf, bw + bh, &inn);
        top = buf;
        max_base_x = 2 * (bw + bh) - 2;
        dx <<= 1;
    } else {
        let fs = if enable { get_filter_strength(wh, 90 - angle, is_sm) } else { 0 };
        if fs != 0 {
            filter_edge(&mut buf, bw + bh, 0, bw + bh, &inn, fs as usize);
            top = buf;
            max_base_x = bw + bh - 1;
        } else {
            max_base_x = bw + bw.min(bh) - 1;
            top = (0..=max_base_x + 1).map(|i| inn(i as i32)).collect();
        }
    }
    let base_inc = 1 + upsample as usize;
    let mut out = vec![0i32; bw * bh];
    let mut xpos = dx;
    for y in 0..bh {
        let frac = xpos & 0x3E;
        let mut base = (xpos >> 6) as usize;
        let mut x = 0;
        while x < bw {
            if base < max_base_x {
                let v = top[base] * (64 - frac) + top[base + 1] * frac;
                out[y * bw + x] = (v + 32) >> 6;
                base += base_inc;
                x += 1;
            } else {
                let fill = top[max_base_x];
                for xx in x..bw {
                    out[y * bw + xx] = fill;
                }
                break;
            }
        }
        xpos += dx;
    }
    out
}

/// Directional Z3 predictor (angle > 180, uses the left edge). `tl`/`corner` as
/// in `z1` (left samples at `corner-1-i`, incl. the bottom-left extension).
/// dav1d `ipred_z3_c` — its negative-stride `left[-base]` reads are flattened
/// here into a forward `left_fwd[base]` array.
pub(super) fn z3(bw: usize, bh: usize, angle_full: i32, tl: &[i32], corner: usize) -> Vec<i32> {
    let is_sm = (angle_full >> 9) & 1 != 0;
    let enable = angle_full >> 10 != 0;
    let angle = angle_full & 511;
    let mut dy = DR_INTRA_DERIVATIVE[((270 - angle) >> 1) as usize];
    let wh = (bw + bh) as i32;
    let from = (bw as i32 - bh as i32).max(0);
    let to = (bw + bh) as i32 + 1;
    let inn = |k: i32| -> i32 {
        let c = k.clamp(from, to - 1);
        tl[(corner as i32 - (bw + bh) as i32 + c) as usize]
    };
    let upsample = enable && get_upsample(wh, angle - 180, is_sm);
    let mut buf = vec![0i32; 2 * (bw + bh) + 16];
    let max_base_y;
    let left_fwd: Vec<i32>;
    if upsample {
        upsample_edge(&mut buf, bw + bh, &inn);
        max_base_y = 2 * (bw + bh) - 2;
        left_fwd = (0..=max_base_y).map(|b| buf[max_base_y - b]).collect();
        dy <<= 1;
    } else {
        let fs = if enable { get_filter_strength(wh, angle - 180, is_sm) } else { 0 };
        if fs != 0 {
            filter_edge(&mut buf, bw + bh, 0, bw + bh, &inn, fs as usize);
            max_base_y = bw + bh - 1;
            left_fwd = (0..=max_base_y).map(|b| buf[max_base_y - b]).collect();
        } else {
            max_base_y = bh + bw.min(bh) - 1;
            left_fwd = (0..=max_base_y)
                .map(|b| tl[corner - 1 - b])
                .collect();
        }
    }
    let base_inc = 1 + upsample as usize;
    let mut out = vec![0i32; bw * bh];
    let mut ypos = dy;
    for x in 0..bw {
        let frac = ypos & 0x3E;
        let base0 = (ypos >> 6) as usize;
        let mut y = 0;
        while y < bh {
            let base = base0 + y * base_inc;
            if base < max_base_y {
                let v = left_fwd[base] * (64 - frac) + left_fwd[base + 1] * frac;
                out[y * bw + x] = (v + 32) >> 6;
                y += 1;
            } else {
                let fill = left_fwd[max_base_y];
                for yy in y..bh {
                    out[yy * bw + x] = fill;
                }
                break;
            }
        }
        ypos += dy;
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
        // IPRED_REF order per size (stride 16): dc, v, h, paeth, smooth,
        // smooth_v, smooth_h, filter0..filter4, z1a, z1b, z3a, z3b.
        for (si, &n) in [4usize, 8].iter().enumerate() {
            let (top, left, tl) = edges(n);
            let base = si * 16;
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
            // Z1 directional: a unified edge buffer (corner + top/topright +
            // left/bottomleft) matching the harness's `setedge`.
            let span = 2 * n;
            let corner = span;
            let mut buf = vec![0i32; 2 * span + 2];
            buf[corner] = 100;
            for i in 0..span {
                buf[corner + 1 + i] = 130 + 3 * i as i32;
                buf[corner - 1 - i] = 110 - 2 * i as i32;
            }
            assert_eq!(z1(n, n, 1083, &buf, corner), IPRED_REF[base + 12], "z1a size {n}");
            assert_eq!(z1(n, n, 1054, &buf, corner), IPRED_REF[base + 13], "z1b size {n}");
            assert_eq!(z3(n, n, 1227, &buf, corner), IPRED_REF[base + 14], "z3a size {n}");
            assert_eq!(z3(n, n, 1249, &buf, corner), IPRED_REF[base + 15], "z3b size {n}");
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
