//! AV1 deblocking loop filter (§7.14) — the leaf edge filter.
//!
//! Faithful 8-bit transcription of dav1d `loop_filter` (`src/loopfilter_tmpl.c`,
//! BSD-2-Clause). Filters `wd`-wide (4/6/8/16) across a single block edge over 4
//! lines: `stride_a` steps to the next of the 4 lines (along the edge), `stride_b`
//! crosses the edge (the `p`/`q` direction). `e`/`i`/`h` are the edge / interior /
//! high-edge-variance thresholds derived from the filter level. The level
//! derivation + per-edge mask building (`lf_mask`/`lf_apply`) land separately.

#![allow(dead_code)]

/// Filter one edge over 4 lines in place. `pos` is the index of `q0` on line 0.
#[allow(clippy::too_many_arguments)]
pub(super) fn loop_filter(
    dst: &mut [u8],
    pos: usize,
    e: i32,
    i: i32,
    h: i32,
    stride_a: isize,
    stride_b: isize,
    wd: i32,
) {
    // 8-bit only: `bitdepth_min_8 = 0`, so `F = 1` and E/I/H are unscaled.
    const F: i32 = 1;
    for line in 0..4isize {
        let base = pos as isize + line * stride_a;
        let at = |k: isize| -> i32 { dst[(base + k * stride_b) as usize] as i32 };

        let (p1, p0, q0, q1) = (at(-2), at(-1), at(0), at(1));
        let mut fm = (p1 - p0).abs() <= i
            && (q1 - q0).abs() <= i
            && (p0 - q0).abs() * 2 + ((p1 - q1).abs() >> 1) <= e;

        let (mut p2, mut p3, mut q2, mut q3) = (0, 0, 0, 0);
        if wd > 4 {
            p2 = at(-3);
            q2 = at(2);
            fm = fm && (p2 - p1).abs() <= i && (q2 - q1).abs() <= i;
            if wd > 6 {
                p3 = at(-4);
                q3 = at(3);
                fm = fm && (p3 - p2).abs() <= i && (q3 - q2).abs() <= i;
            }
        }
        if !fm {
            continue;
        }

        let (mut p6, mut p5, mut p4, mut q4, mut q5, mut q6) = (0, 0, 0, 0, 0, 0);
        let mut flat8out = false;
        if wd >= 16 {
            p6 = at(-7);
            p5 = at(-6);
            p4 = at(-5);
            q4 = at(4);
            q5 = at(5);
            q6 = at(6);
            flat8out = (p6 - p0).abs() <= F
                && (p5 - p0).abs() <= F
                && (p4 - p0).abs() <= F
                && (q4 - q0).abs() <= F
                && (q5 - q0).abs() <= F
                && (q6 - q0).abs() <= F;
        }
        let mut flat8in = false;
        if wd >= 6 {
            flat8in = (p2 - p0).abs() <= F
                && (p1 - p0).abs() <= F
                && (q1 - q0).abs() <= F
                && (q2 - q0).abs() <= F;
        }
        if wd >= 8 {
            flat8in = flat8in && (p3 - p0).abs() <= F && (q3 - q0).abs() <= F;
        }

        let mut set = |k: isize, v: i32| {
            dst[(base + k * stride_b) as usize] = v as u8;
        };

        if wd >= 16 && flat8out && flat8in {
            set(-6, (p6 + p6 + p6 + p6 + p6 + p6 * 2 + p5 * 2 + p4 * 2 + p3 + p2 + p1 + p0 + q0 + 8) >> 4);
            set(-5, (p6 + p6 + p6 + p6 + p6 + p5 * 2 + p4 * 2 + p3 * 2 + p2 + p1 + p0 + q0 + q1 + 8) >> 4);
            set(-4, (p6 + p6 + p6 + p6 + p5 + p4 * 2 + p3 * 2 + p2 * 2 + p1 + p0 + q0 + q1 + q2 + 8) >> 4);
            set(-3, (p6 + p6 + p6 + p5 + p4 + p3 * 2 + p2 * 2 + p1 * 2 + p0 + q0 + q1 + q2 + q3 + 8) >> 4);
            set(-2, (p6 + p6 + p5 + p4 + p3 + p2 * 2 + p1 * 2 + p0 * 2 + q0 + q1 + q2 + q3 + q4 + 8) >> 4);
            set(-1, (p6 + p5 + p4 + p3 + p2 + p1 * 2 + p0 * 2 + q0 * 2 + q1 + q2 + q3 + q4 + q5 + 8) >> 4);
            set(0, (p5 + p4 + p3 + p2 + p1 + p0 * 2 + q0 * 2 + q1 * 2 + q2 + q3 + q4 + q5 + q6 + 8) >> 4);
            set(1, (p4 + p3 + p2 + p1 + p0 + q0 * 2 + q1 * 2 + q2 * 2 + q3 + q4 + q5 + q6 + q6 + 8) >> 4);
            set(2, (p3 + p2 + p1 + p0 + q0 + q1 * 2 + q2 * 2 + q3 * 2 + q4 + q5 + q6 + q6 + q6 + 8) >> 4);
            set(3, (p2 + p1 + p0 + q0 + q1 + q2 * 2 + q3 * 2 + q4 * 2 + q5 + q6 + q6 + q6 + q6 + 8) >> 4);
            set(4, (p1 + p0 + q0 + q1 + q2 + q3 * 2 + q4 * 2 + q5 * 2 + q6 + q6 + q6 + q6 + q6 + 8) >> 4);
            set(5, (p0 + q0 + q1 + q2 + q3 + q4 * 2 + q5 * 2 + q6 * 2 + q6 + q6 + q6 + q6 + q6 + 8) >> 4);
        } else if wd >= 8 && flat8in {
            set(-3, (p3 + p3 + p3 + 2 * p2 + p1 + p0 + q0 + 4) >> 3);
            set(-2, (p3 + p3 + p2 + 2 * p1 + p0 + q0 + q1 + 4) >> 3);
            set(-1, (p3 + p2 + p1 + 2 * p0 + q0 + q1 + q2 + 4) >> 3);
            set(0, (p2 + p1 + p0 + 2 * q0 + q1 + q2 + q3 + 4) >> 3);
            set(1, (p1 + p0 + q0 + 2 * q1 + q2 + q3 + q3 + 4) >> 3);
            set(2, (p0 + q0 + q1 + 2 * q2 + q3 + q3 + q3 + 4) >> 3);
        } else if wd == 6 && flat8in {
            set(-2, (p2 + 2 * p2 + 2 * p1 + 2 * p0 + q0 + 4) >> 3);
            set(-1, (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3);
            set(0, (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3);
            set(1, (p0 + 2 * q0 + 2 * q1 + 2 * q2 + q2 + 4) >> 3);
        } else {
            let hev = (p1 - p0).abs() > h || (q1 - q0).abs() > h;
            // `iclip_diff` at 8-bit: clamp to [-128, 127].
            let clip = |v: i32| v.clamp(-128, 127);
            if hev {
                let f0 = clip(p1 - q1);
                let f = clip(3 * (q0 - p0) + f0);
                let f1 = (f + 4).min(127) >> 3;
                let f2 = (f + 3).min(127) >> 3;
                set(-1, (p0 + f2).clamp(0, 255));
                set(0, (q0 - f1).clamp(0, 255));
            } else {
                let f = clip(3 * (q0 - p0));
                let f1 = (f + 4).min(127) >> 3;
                let f2 = (f + 3).min(127) >> 3;
                set(-1, (p0 + f2).clamp(0, 255));
                set(0, (q0 - f1).clamp(0, 255));
                let f3 = (f1 + 1) >> 1;
                set(-2, (p1 + f3).clamp(0, 255));
                set(1, (q1 - f3).clamp(0, 255));
            }
        }
    }
}

/// Per-level edge (`e`) / interior (`i`) limits, derived from the frame's filter
/// sharpness. dav1d `dav1d_calc_eih` (`src/lf_mask.c`). `H` (high-edge-variance)
/// is `level >> 4` at apply time; not stored here.
pub(super) struct FilterLut {
    pub e: [i32; 64],
    pub i: [i32; 64],
}

/// Build the `FilterLut` for a given `filter_sharpness` (0..=7).
pub(super) fn calc_eih(filter_sharpness: u32) -> FilterLut {
    let sharp = filter_sharpness as i32;
    let mut lut = FilterLut { e: [0; 64], i: [0; 64] };
    for level in 0..64i32 {
        let mut limit = level;
        if sharp > 0 {
            limit >>= (sharp + 3) >> 2;
            limit = limit.min(9 - sharp);
        }
        limit = limit.max(1);
        lut.i[level as usize] = limit;
        lut.e[level as usize] = 2 * (level + 2) + limit;
    }
    lut
}

/// Intra deblock level for one (plane, direction) — dav1d `calc_lf_value` on the
/// `INTRA_FRAME` (ref 0) path: `base = clip(clip(base_lvl + lf_delta) + seg_delta)`,
/// then `clip(base + ref_delta[0] << (base>=32))` when the deltas are enabled.
/// `base_lvl` is `loop_filter_level[plane_dir]`. Returns 0..=63.
pub(super) fn lf_level(
    base_lvl: i32,
    lf_delta: i32,
    seg_delta: i32,
    ref_delta0: i32,
    delta_enabled: bool,
) -> u8 {
    let base = ((base_lvl + lf_delta).clamp(0, 63) + seg_delta).clamp(0, 63);
    if !delta_enabled {
        return base as u8;
    }
    let sh = (base >= 32) as i32;
    (base + ref_delta0 * (1 << sh)).clamp(0, 63) as u8
}

/// Filter width for an edge, from the minimum transform dimension across it
/// (in pixels) — dav1d `loop_filter_{h,v}_sb128{y,uv}_c`. Luma uses `4 << idx`
/// (4/8/16) with idx from the txdim≥8 / txdim≥16 masks; chroma uses `4 + 2*idx`
/// (4/6) from the txdim≥8 mask only. A 0 input (uncovered cell) yields 4.
pub(super) fn lf_wd(min_dim_px: i32, is_chroma: bool) -> i32 {
    if is_chroma {
        if min_dim_px >= 8 {
            6
        } else {
            4
        }
    } else if min_dim_px >= 16 {
        16
    } else if min_dim_px >= 8 {
        8
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("lf_ref.rs");

    #[test]
    fn loop_filter_matches_dav1d() {
        // Mirror the harness: 4 lines × 16 samples, edge between index 7 (p0) and
        // 8 (q0); `dst` at q0 of line 0 (pos = 8). stride_a = 16, stride_b = 1.
        let lines: [[u8; 16]; 4] = [
            [30, 34, 38, 42, 46, 50, 54, 58, 92, 96, 100, 104, 108, 112, 116, 120],
            [40, 40, 40, 40, 40, 40, 40, 40, 200, 200, 200, 200, 200, 200, 200, 200],
            [70, 71, 72, 72, 73, 73, 74, 74, 78, 79, 79, 80, 81, 82, 82, 83],
            [10, 90, 20, 130, 60, 15, 200, 64, 70, 240, 12, 180, 33, 99, 5, 150],
        ];
        let widths = [4i32, 6, 8, 16];
        let thr = [[64i32, 32, 16], [255, 64, 8], [120, 16, 4]];
        let mut ri = 0usize;
        for &wd in widths.iter() {
            for t in thr.iter() {
                let mut buf = [0u8; 64];
                for (l, ln) in lines.iter().enumerate() {
                    buf[l * 16..l * 16 + 16].copy_from_slice(ln);
                }
                loop_filter(&mut buf, 8, t[0], t[1], t[2], 16, 1, wd);
                let got: Vec<i32> = buf.iter().map(|&b| b as i32).collect();
                assert_eq!(got, LF_REF[ri], "wd={wd} E={} I={} H={}", t[0], t[1], t[2]);
                ri += 1;
            }
        }
    }

    #[test]
    fn calc_eih_matches_spec() {
        // Golden values hand-derived from dav1d's `calc_eih` arithmetic.
        let s0 = calc_eih(0);
        assert_eq!((s0.i[0], s0.e[0]), (1, 5)); // limit=max(1,0)=1, e=2*2+1
        assert_eq!((s0.i[10], s0.e[10]), (10, 34)); // limit=10, e=2*12+10
        assert_eq!((s0.i[63], s0.e[63]), (63, 193)); // e=2*65+63
        let s2 = calc_eih(2);
        // level 20: 20>>((2+3)>>2=1)=10, min(10,9-2=7)=7, max(1,7)=7; e=2*22+7
        assert_eq!((s2.i[20], s2.e[20]), (7, 51));
        let s7 = calc_eih(7);
        // level 63: 63>>((7+3)>>2=2)=15, min(15,9-7=2)=2; e=2*65+2
        assert_eq!((s7.i[63], s7.e[63]), (2, 132));
        // sharpness clamps the interior limit but never below 1.
        assert_eq!(s7.i[0], 1);
    }

    #[test]
    fn lf_level_intra_derivation() {
        // No deltas → base (clamped).
        assert_eq!(lf_level(16, 0, 0, 99, false), 16);
        assert_eq!(lf_level(70, 0, 0, 0, false), 63); // base clamps to 63
        // delta enabled, base<32 → +ref_delta0*1.
        assert_eq!(lf_level(16, 0, 0, 1, true), 17);
        // delta enabled, base>=32 → +ref_delta0*2 (sh=1).
        assert_eq!(lf_level(40, 0, 0, 1, true), 42);
        // negative ref_delta clamps at 0.
        assert_eq!(lf_level(1, 0, 0, -4, true), 0);
        // seg + lf deltas fold into base before the ref delta.
        assert_eq!(lf_level(20, 4, 2, 0, true), 26);
    }
}
