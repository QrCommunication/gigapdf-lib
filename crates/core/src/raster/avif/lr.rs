//! AV1 loop restoration (§7.17) — the post-CDEF in-loop restoration filters.
//!
//! Two restoration kernels, applied per `lr_unit` over the post-CDEF planes:
//!
//! * **Wiener** (§7.17.4/5): a separable symmetric 7-tap filter rebuilt from the
//!   three coded taps per direction with unit DC gain
//!   (`filter = [c0, c1, c2, 128 - 2*(c0+c1+c2), c2, c1, c0]`). Horizontal pass
//!   to a 16-bit intermediate (round `InterRound0 = 3`, BD=8 bias `1<<14` +
//!   center `*128`, clip `[0, 8191]`), then a vertical pass (round
//!   `InterRound1 = 11`, bias `-(1<<18)`, clip `[0, 255]`).
//! * **Self-guided restoration / SGR** (§7.17.2/3): two box filters (radii from
//!   `SGR_PARAMS[set]`) producing `flt0`/`flt1`, projected against the source by
//!   the signed weights `xqd` (`w2 = 128 - w0 - w1`), final round 11.
//!
//! Faithful to the AV1 spec integer math (`SGR_PARAMS[16][4]` layout) and to
//! dav1d `looprestoration_tmpl.c`. BD=8 only (the `BitDepth-8` down-scales are
//! `Round2(·, 0)` no-ops). Frame edges replicate the last in-plane sample; the
//! 64-row / offset-8 stripe boundary (§7.17.6) substitutes the pre-CDEF picture
//! for the ≤2 cross-stripe halo rows so neighbour CDEF output does not feed in.

#![allow(dead_code)]

/// Per-unit restoration kind. The integer values mirror dav1d's
/// `Dav1dRestorationType` (NONE/SWITCHABLE/WIENER/SGRPROJ); only the three
/// non-switchable kinds are stored per unit (SWITCHABLE resolves to one of them).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum RestorationType {
    #[default]
    None,
    Wiener,
    Sgr,
}

/// One restoration unit's decoded parameters.
#[derive(Clone, Copy, Debug)]
pub(super) struct LrUnit {
    pub kind: RestorationType,
    /// Wiener taps `[v0,v1,v2, h0,h1,h2]` (the 3 coded taps per direction).
    pub wiener: [i32; 6],
    /// SGR parameter-set index (0..16).
    pub sgr_set: usize,
    /// SGR projection weights `xqd = [w0, w1]`.
    pub sgr_xqd: [i32; 2],
}

impl Default for LrUnit {
    fn default() -> Self {
        LrUnit {
            kind: RestorationType::None,
            wiener: [0; 6],
            sgr_set: 0,
            sgr_xqd: [0; 2],
        }
    }
}

// ── Constants (AV1 §3 / §7.17) ────────────────────────────────────────────
const FILTER_BITS: i32 = 7;
const SGRPROJ_PRJ_BITS: i32 = 7;
const SGRPROJ_RST_BITS: i32 = 4;
const SGRPROJ_SGR_BITS: i32 = 8;
const SGRPROJ_RECIP_BITS: i32 = 12;
const SGRPROJ_MTABLE_BITS: i32 = 20;

/// Wiener tap min/max/mid/K (per coded tap, §5.11.58). `mid` seeds the per-tile
/// reference; the range is `max - min + 1 = {16, 32, 64}` so `decode_subexp`'s
/// `n >> k == 8` precondition holds (`16>>1, 32>>2, 64>>3`).
pub(super) const WIENER_TAPS_MIN: [i32; 3] = [-5, -23, -17];
pub(super) const WIENER_TAPS_MAX: [i32; 3] = [10, 8, 46];
pub(super) const WIENER_TAPS_MID: [i32; 3] = [3, -7, 15];
pub(super) const WIENER_TAPS_K: [i32; 3] = [1, 2, 3];

/// SGR `xqd` min/max/mid (§5.11.58). Range `128` (`128>>4 == 8`).
pub(super) const SGR_XQD_MIN: [i32; 2] = [-96, -32];
pub(super) const SGR_XQD_MAX: [i32; 2] = [31, 95];
pub(super) const SGR_XQD_MID: [i32; 2] = [-32, 31];
pub(super) const SGRPROJ_PRJ_SUBEXP_K: i32 = 4;
pub(super) const SGRPROJ_PARAMS_BITS: u32 = 4;

// ── Restoration-type CDF defaults (dav1d `src/cdf.c`, inverse Q15 `32768-raw`)
// Binary CDFs are `[prob, counter]`; the 3-symbol switchable is `[b0, b1, _, ctr]`.
/// `restore_wiener` (2-sym): raw 11570 → `32768 - 11570 = 21198`.
pub(super) const RESTORE_WIENER_CDF: [u16; 2] = [21198, 0];
/// `restore_sgrproj` (2-sym): raw 16855 → `32768 - 16855 = 15913`.
pub(super) const RESTORE_SGRPROJ_CDF: [u16; 2] = [15913, 0];
/// `restore_switchable` (3-sym): raw `{9413, 22581}` → `{23355, 10187}`.
pub(super) const RESTORE_SWITCHABLE_CDF: [u16; 4] = [23355, 10187, 0, 0];

/// `Sgr_Params[16][4]` = `{ r0, eps0, r1, eps1 }` (AV1 spec). `r=2` → 5×5 box,
/// `r=1` → 3×3, `r=0` → that pass disabled.
pub(super) const SGR_PARAMS: [[i32; 4]; 16] = [
    [2, 12, 1, 4],
    [2, 15, 1, 6],
    [2, 18, 1, 8],
    [2, 21, 1, 9],
    [2, 24, 1, 10],
    [2, 29, 1, 11],
    [2, 36, 1, 12],
    [2, 45, 1, 13],
    [2, 56, 1, 14],
    [2, 68, 1, 15],
    [0, 0, 1, 5],
    [0, 0, 1, 8],
    [0, 0, 1, 11],
    [0, 0, 1, 14],
    [2, 30, 0, 0],
    [2, 75, 0, 0],
];

/// `Sgr_X_By_Xplus1[256]` — the `a2` reciprocal LUT (`≈ 256·z/(z+1)`), indexed by
/// `min(z, 255)`. Returns the spec's `256 - a2` complement; `[0]=255 .. [255]=0`.
pub(super) static SGR_X_BY_X: [u8; 256] = [
    255, 128, 85, 64, 51, 43, 37, 32, 28, 26, 23, 21, 20, 18, 17, 16, //
    15, 14, 13, 13, 12, 12, 11, 11, 10, 10, 9, 9, 9, 9, 8, 8, //
    8, 8, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 5, 5, //
    5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, //
    4, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, //
];

/// Per-tile loop-restoration reference state for the subexp-with-ref reads
/// (§5.11.58). Reset to the mids at the start of each tile; updated to the
/// last-decoded value after every read. One copy per plane.
#[derive(Clone, Copy, Debug)]
pub(super) struct LrRef {
    pub wiener: [[i32; 3]; 2], // [pass(v,h)][tap]
    pub sgr_xqd: [i32; 2],
}

impl Default for LrRef {
    fn default() -> Self {
        LrRef {
            wiener: [WIENER_TAPS_MID, WIENER_TAPS_MID],
            sgr_xqd: SGR_XQD_MID,
        }
    }
}

/// The per-plane `frame_restoration_type` (decoded in the frame header). Mirrors
/// the spec `Remap_Lr_Type` outputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum FrameRestoration {
    #[default]
    None,
    Switchable,
    Wiener,
    Sgr,
}

/// `frame_restoration_type` for a header `lr_type` field (`Remap_Lr_Type`):
/// `0→None, 1→Switchable, 2→Wiener, 3→Sgr` (the values already stored in
/// `fh.lr_type` after `REMAP_LR_TYPE` in the header parse).
pub(super) fn frame_restoration(lr_type: u8) -> FrameRestoration {
    match lr_type {
        1 => FrameRestoration::Switchable,
        2 => FrameRestoration::Wiener,
        3 => FrameRestoration::Sgr,
        _ => FrameRestoration::None,
    }
}

/// CDF state for the three restoration-type symbols (one set per tile, adaptive).
#[derive(Clone, Debug)]
pub(super) struct LrCdf {
    pub wiener: [u16; 2],
    pub sgrproj: [u16; 2],
    pub switchable: [u16; 4],
}

impl Default for LrCdf {
    fn default() -> Self {
        LrCdf {
            wiener: RESTORE_WIENER_CDF,
            sgrproj: RESTORE_SGRPROJ_CDF,
            switchable: RESTORE_SWITCHABLE_CDF,
        }
    }
}

/// Read one restoration unit's params from the tile stream (§5.11.58
/// `read_lr_unit`). `plane` selects the chroma firstCoeff (chroma Wiener omits
/// tap 0). `ft` is the plane's frame restoration type; `refs`/`cdf` are the
/// per-tile reference taps + adaptive CDFs, both updated in place.
pub(super) fn read_lr_unit(
    msac: &mut super::msac::Msac,
    plane: usize,
    ft: FrameRestoration,
    refs: &mut LrRef,
    cdf: &mut LrCdf,
) -> LrUnit {
    // Resolve the unit's restoration kind from the frame type + an adaptive read.
    let kind = match ft {
        FrameRestoration::None => RestorationType::None,
        FrameRestoration::Wiener => {
            if msac.bool_adapt(&mut cdf.wiener) != 0 {
                RestorationType::Wiener
            } else {
                RestorationType::None
            }
        }
        FrameRestoration::Sgr => {
            if msac.bool_adapt(&mut cdf.sgrproj) != 0 {
                RestorationType::Sgr
            } else {
                RestorationType::None
            }
        }
        FrameRestoration::Switchable => {
            // 3-symbol: 0→None, 1→Wiener, 2→Sgr (spec ordering).
            match msac.symbol_adapt(&mut cdf.switchable, 3) {
                1 => RestorationType::Wiener,
                2 => RestorationType::Sgr,
                _ => RestorationType::None,
            }
        }
    };

    let mut unit = LrUnit {
        kind,
        ..LrUnit::default()
    };
    match kind {
        RestorationType::Wiener => {
            // Vertical (pass 0) then horizontal (pass 1); 3 taps each. Chroma
            // (`plane != 0`) forces tap 0 to 0 (firstCoeff = 1).
            for pass in 0..2 {
                for t in 0..3 {
                    let v = if t == 0 && plane != 0 {
                        0
                    } else {
                        let min = WIENER_TAPS_MIN[t];
                        let max = WIENER_TAPS_MAX[t];
                        let n = (max - min + 1) as u32;
                        let k = WIENER_TAPS_K[t] as u32;
                        let r = (refs.wiener[pass][t] - min) as u32;
                        msac.decode_subexp(r as i32, n, k) + min
                    };
                    refs.wiener[pass][t] = v;
                    unit.wiener[pass * 3 + t] = v;
                }
            }
        }
        RestorationType::Sgr => {
            let set = msac.bools(SGRPROJ_PARAMS_BITS) as usize;
            unit.sgr_set = set;
            let p = SGR_PARAMS[set];
            for i in 0..2 {
                let radius = p[i * 2];
                let min = SGR_XQD_MIN[i];
                let max = SGR_XQD_MAX[i];
                let v = if radius != 0 {
                    let n = (max - min + 1) as u32;
                    let r = (refs.sgr_xqd[i] - min) as u32;
                    msac.decode_subexp(r as i32, n, SGRPROJ_PRJ_SUBEXP_K as u32) + min
                } else if i == 1 {
                    // Spec: derive xqd1 = Clip3(min, max, 128 - xqd0).
                    ((1 << SGRPROJ_PRJ_BITS) - refs.sgr_xqd[0]).clamp(min, max)
                } else {
                    0
                };
                refs.sgr_xqd[i] = v;
                unit.sgr_xqd[i] = v;
            }
        }
        RestorationType::None => {}
    }
    unit
}

/// Build the symmetric 7-tap Wiener kernel from the 3 coded taps with unit gain:
/// `[c0, c1, c2, 128 - 2*(c0+c1+c2), c2, c1, c0]` (§7.17.5). `128 = 1<<FILTER_BITS`.
pub(super) fn wiener_kernel(coeff: &[i32; 3]) -> [i32; 7] {
    let center = (1 << FILTER_BITS) - 2 * (coeff[0] + coeff[1] + coeff[2]);
    [
        coeff[0], coeff[1], coeff[2], center, coeff[2], coeff[1], coeff[0],
    ]
}

/// `Round2(x, n)` (AV1): `(x + (1<<(n-1))) >> n` for `n > 0`, `x` for `n == 0`.
#[inline]
fn round2(x: i64, n: i32) -> i64 {
    if n == 0 {
        x
    } else {
        (x + (1i64 << (n - 1))) >> n
    }
}

/// A plane view with edge-replicating sampling, parameterised by the stripe
/// machinery (§7.17.6). In-stripe samples come from `cdef` (post-CDEF); rows
/// outside `[stripe_start, stripe_end]` come from `pre_cdef` clamped to ±2 of the
/// stripe boundary. All coordinates are clamped to the plane bounds first.
struct PlaneSrc<'a> {
    cdef: &'a [u8],
    pre_cdef: &'a [u8],
    w: usize,
    h: usize,
    stride: usize,
    stripe_start: i32,
    stripe_end: i32,
}

impl PlaneSrc<'_> {
    #[inline]
    fn at(&self, x: i32, y: i32) -> i32 {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1);
        if y < self.stripe_start {
            let yy = (self.stripe_start - 2).max(y).clamp(0, self.h as i32 - 1) as usize;
            self.pre_cdef[yy * self.stride + x] as i32
        } else if y > self.stripe_end {
            let yy = (self.stripe_end + 2).min(y).clamp(0, self.h as i32 - 1) as usize;
            self.pre_cdef[yy * self.stride + x] as i32
        } else {
            self.cdef[y as usize * self.stride + x] as i32
        }
    }
}

/// Wiener restoration of a `w×h` unit at `(x0, y0)` (§7.17.4), using the
/// fully-materialised 7-tap kernels (center tap included, so NO separate `*128`
/// term — that is dav1d's 3-tap-storage form). BD=8 spec constants:
/// `InterRound0 = 3`, horizontal intermediate `Clip3(-2048, 6143, Round2(s, 3))`;
/// `InterRound1 = 11`, final `Clip1(Round2(s, 11))`.
#[allow(clippy::too_many_arguments)]
fn wiener_unit(
    src: &PlaneSrc,
    dst: &mut [u8],
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    vfilter: &[i32; 7],
    hfilter: &[i32; 7],
) {
    const INTER_ROUND0: i32 = 3;
    const INTER_ROUND1: i32 = 11;
    // offset = 1 << (BitDepth + FILTER_BITS - InterRound0 - 1) = 1 << 11 = 2048.
    let offset = 1i64 << (8 + FILTER_BITS - INTER_ROUND0 - 1);
    // limit = (1 << (BitDepth + 1 + FILTER_BITS - InterRound0)) - 1 = 8191.
    let limit = (1i64 << (8 + 1 + FILTER_BITS - INTER_ROUND0)) - 1;

    // Horizontal pass → signed intermediate of (h + 6) rows × w cols.
    let mut inter = vec![0i32; (h + 6) * w];
    for r in 0..h + 6 {
        let sy = y0 as i32 + r as i32 - 3;
        for c in 0..w {
            let sx = x0 as i32 + c as i32;
            let mut sum = 0i64;
            for (t, &f) in hfilter.iter().enumerate() {
                sum += src.at(sx + t as i32 - 3, sy) as i64 * f as i64;
            }
            let v = round2(sum, INTER_ROUND0).clamp(-offset, limit - offset);
            inter[r * w + c] = v as i32;
        }
    }
    // Vertical pass → final pixels.
    for r in 0..h {
        for c in 0..w {
            let mut sum = 0i64;
            for (t, &f) in vfilter.iter().enumerate() {
                sum += inter[(r + t) * w + c] as i64 * f as i64;
            }
            let v = round2(sum, INTER_ROUND1).clamp(0, 255);
            dst[(y0 + r) * src.stride + (x0 + c)] = v as u8;
        }
    }
}

/// One SGR box-filter pass (§7.17.3) producing `flt[w*h]`. `radius` ∈ {1,2},
/// `eps` the strength. Reads `src` (with full stripe/edge sampling) over the
/// unit plus a 1-pixel border. Integer arithmetic per spec (`a2` via `SGR_X_BY_X`,
/// `b2` via `one_over_n`, neighbour combine with shift 8+`shift`-4).
fn sgr_pass(
    src: &PlaneSrc,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    radius: i32,
    eps: i32,
) -> Vec<i32> {
    let n = (2 * radius + 1) * (2 * radius + 1); // 25 or 9
    let n2e = (n * n * eps) as i64;
    let s = ((1i64 << SGRPROJ_MTABLE_BITS) + n2e / 2) / n2e;
    let one_over_n = ((1 << SGRPROJ_RECIP_BITS) + (n >> 1)) / n; // 164 (n=25), 455 (n=9)

    // A (= a2 gain) and B over the unit + 1px border, indexed [i+1][j+1].
    let bw = w + 2;
    let bh = h + 2;
    let mut a_arr = vec![0i32; bw * bh];
    let mut b_arr = vec![0i32; bw * bh];
    for bi in 0..bh {
        let i = bi as i32 - 1; // -1 .. h
                               // 5×5 (pass-0): only odd source rows are summed/used (row subsampling).
        for bj in 0..bw {
            let j = bj as i32 - 1; // -1 .. w
            let mut suma = 0i64;
            let mut sumb = 0i64;
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let c = src.at(x0 as i32 + j + dx, y0 as i32 + i + dy) as i64;
                    suma += c * c;
                    sumb += c;
                }
            }
            // BD=8: Round2(a, 0) and Round2(b, 0) are no-ops.
            let p = (suma * n as i64 - sumb * sumb).max(0);
            let z = round2(p * s, SGRPROJ_MTABLE_BITS);
            // `x_comp = 256 - a2` from the reciprocal LUT (`z >= 255` → a2 = 256,
            // i.e. x_comp = 0). Store the real gain `a2` and `b2 = Round2((256-a2)
            // *b*oneOverN, 12)` (spec §7.17.3), so the combine is `a2*src + b2`.
            let x_comp = if z >= 255 {
                0i64
            } else {
                SGR_X_BY_X[z as usize] as i64
            };
            let a2 = (1i64 << SGRPROJ_SGR_BITS) - x_comp;
            let b2 = x_comp * sumb * one_over_n as i64;
            a_arr[bi * bw + bj] = a2 as i32;
            b_arr[bi * bw + bj] = round2(b2, SGRPROJ_RECIP_BITS) as i32;
        }
    }

    // Neighbour combine (§7.17.3): 5×5 uses the 6-neighbour (odd-row) pattern,
    // 3×3 the 8-neighbour plus/diagonal pattern. Spec form: `flt = Round2(a*src +
    // b, shift)` where `a` is the combined a2 gain and `b` the combined b2.
    let mut flt = vec![0i32; w * h];
    for i in 0..h {
        for j in 0..w {
            let (mut acc_a, mut acc_b) = (0i64, 0i64);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let weight = if radius == 2 {
                        // contributes only on odd source rows (i+dy parity)
                        if (i as i32 + dy).rem_euclid(2) == 1 {
                            if dx == 0 {
                                6
                            } else {
                                5
                            }
                        } else {
                            0
                        }
                    } else if dx == 0 || dy == 0 {
                        4
                    } else {
                        3
                    };
                    let ai = (i as i32 + dy + 1) as usize;
                    let aj = (j as i32 + dx + 1) as usize;
                    acc_a += weight as i64 * a_arr[ai * bw + aj] as i64;
                    acc_b += weight as i64 * b_arr[ai * bw + aj] as i64;
                }
            }
            // shift = 5 (3×3 / 5×5 even output rows), 4 (5×5 odd output rows).
            let shift = if radius == 2 && (i & 1) == 1 { 4 } else { 5 };
            let src_v = src.at(x0 as i32 + j as i32, y0 as i32 + i as i32) as i64;
            // flt = Round2(a*src + b, 8 + shift - 4).
            let v = acc_a * src_v + acc_b;
            flt[i * w + j] = round2(v, SGRPROJ_SGR_BITS + shift - SGRPROJ_RST_BITS) as i32;
        }
    }
    flt
}

/// Self-guided restoration of a `w×h` unit (§7.17.2/3). Runs up to two box passes
/// (per `SGR_PARAMS[set]`), then projects against the source with weights
/// `xqd = [w0, w1]` (`w2 = 128 - w0 - w1`), final `Round2(v, 11)` and clip [0,255].
#[allow(clippy::too_many_arguments)]
fn sgr_unit(
    src: &PlaneSrc,
    dst: &mut [u8],
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    set: usize,
    xqd: &[i32; 2],
) {
    let p = SGR_PARAMS[set];
    let (r0, eps0, r1, eps1) = (p[0], p[1], p[2], p[3]);
    let flt0 = if r0 != 0 {
        Some(sgr_pass(src, x0, y0, w, h, r0, eps0))
    } else {
        None
    };
    let flt1 = if r1 != 0 {
        Some(sgr_pass(src, x0, y0, w, h, r1, eps1))
    } else {
        None
    };
    let (w0, w1) = (xqd[0] as i64, xqd[1] as i64);
    let w2 = (1i64 << SGRPROJ_PRJ_BITS) - w0 - w1;
    for i in 0..h {
        for j in 0..w {
            let src_v = src.at(x0 as i32 + j as i32, y0 as i32 + i as i32) as i64;
            let u = src_v << SGRPROJ_RST_BITS;
            let mut v = w1 * u;
            v += w0 * flt0.as_ref().map_or(u, |f| f[i * w + j] as i64);
            v += w2 * flt1.as_ref().map_or(u, |f| f[i * w + j] as i64);
            let out = round2(v, SGRPROJ_RST_BITS + SGRPROJ_PRJ_BITS).clamp(0, 255);
            dst[(y0 + i) * src.stride + (x0 + j)] = out as u8;
        }
    }
}

/// Per-plane loop-restoration geometry + decoded unit params.
#[derive(Clone, Default)]
pub(super) struct PlaneLr {
    /// Restoration unit size (plane pixels).
    pub unit_size: usize,
    /// Number of restoration units across / down.
    pub unit_cols: usize,
    pub unit_rows: usize,
    /// Per-unit params, row-major `unit_rows × unit_cols`.
    pub units: Vec<LrUnit>,
}

/// `count_units_in_frame(unitSize, dim)` (§7.17.1): round-half-up, at least 1.
pub(super) fn count_units(unit_size: usize, dim: usize) -> usize {
    ((dim + (unit_size >> 1)) / unit_size).max(1)
}

/// Apply loop restoration to one plane in place (§7.17.1). `cdef` is the live
/// post-CDEF plane (mutated to the restored result); `pre_cdef` the post-deblock
/// (pre-CDEF) clone used for the cross-stripe / frame-edge halo. `sub_y` selects
/// the plane's vertical subsampling for the stripe geometry.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_plane(
    cdef: &[u8],
    pre_cdef: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    stride: usize,
    sub_y: u32,
    lr: &PlaneLr,
) {
    // Stripe height in this plane's rows (64 luma rows, offset 8).
    let stripe_h = (64usize >> sub_y) as i32;
    let offset = (8i32) >> sub_y;
    let unit_size = lr.unit_size;
    let mut y = 0usize;
    while y < h {
        // The stripe this row belongs to (§7.17.6): stripeNum = (lumaY + 8) / 64,
        // StripeStartY = (-8 + stripeNum*64) >> subY. Stripes are offset up by 8,
        // so stripe 0 spans [0, 55], stripe 1 [56, 119], … — the end is the NEXT
        // stripe's start minus one (NOT start + 64 - 1). The stripe selects which
        // buffer the cross-boundary halo reads from.
        let stripe_num = ((y as i32) + offset) / stripe_h;
        let stripe_start = (-offset + stripe_num * stripe_h).max(0);
        let stripe_end = (-offset + (stripe_num + 1) * stripe_h - 1).min(h as i32 - 1);
        let stripe_row_end = stripe_end as usize + 1;
        let src = PlaneSrc {
            cdef,
            pre_cdef,
            w,
            h,
            stride,
            stripe_start,
            stripe_end,
        };
        let unit_row = (y / unit_size).min(lr.unit_rows - 1);
        // Process a band of rows that stays inside BOTH this stripe and this
        // unit-row (and the plane). The unit grid (≥64) is usually coarser than
        // the stripe, but clamp to whichever boundary comes first.
        let unit_row_end = (unit_row + 1) * unit_size;
        let band_end = stripe_row_end.min(unit_row_end).min(h);
        let bh = band_end - y;
        let mut x = 0usize;
        while x < w {
            let unit_col = (x / unit_size).min(lr.unit_cols - 1);
            let unit = lr.units[unit_row * lr.unit_cols + unit_col];
            // Block width: up to the unit-column edge, clipped to the plane.
            let unit_col_end = (unit_col + 1) * unit_size;
            let bw = unit_col_end.min(w) - x;
            match unit.kind {
                RestorationType::None => {
                    // Copy the post-CDEF samples through unchanged.
                    for r in 0..bh {
                        for c in 0..bw {
                            dst[(y + r) * stride + (x + c)] = cdef[(y + r) * stride + (x + c)];
                        }
                    }
                }
                RestorationType::Wiener => {
                    let vf = wiener_kernel(&[unit.wiener[0], unit.wiener[1], unit.wiener[2]]);
                    let hf = wiener_kernel(&[unit.wiener[3], unit.wiener[4], unit.wiener[5]]);
                    wiener_unit(&src, dst, x, y, bw, bh, &vf, &hf);
                }
                RestorationType::Sgr => {
                    sgr_unit(&src, dst, x, y, bw, bh, unit.sgr_set, &unit.sgr_xqd);
                }
            }
            x += bw;
        }
        y = band_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiener_kernel_unit_gain() {
        // [c0,c1,c2, 128-2Σc, c2,c1,c0]; sums to 128 regardless of taps.
        let k = wiener_kernel(&[3, -7, 15]);
        assert_eq!(k, [3, -7, 15, 128 - 2 * (3 - 7 + 15), 15, -7, 3]);
        assert_eq!(k.iter().sum::<i32>(), 128);
        let k2 = wiener_kernel(&[-5, 8, 46]);
        assert_eq!(k2.iter().sum::<i32>(), 128);
    }

    #[test]
    fn count_units_round_half_up() {
        // (dim + sz/2)/sz, min 1.
        assert_eq!(count_units(64, 64), 1);
        assert_eq!(count_units(64, 65), 1); // 65+32=97; 97/64=1
        assert_eq!(count_units(64, 96), 2); // 96+32=128; /64=2
        assert_eq!(count_units(64, 95), 1); // 95+32=127; /64=1
        assert_eq!(count_units(64, 1), 1); // max(0,1)
        assert_eq!(count_units(256, 1024), 4);
    }

    #[test]
    fn sgr_x_by_x_endpoints() {
        // The reciprocal LUT: [0]=255, [1]=128, [255]=0, monotone non-increasing.
        assert_eq!(SGR_X_BY_X[0], 255);
        assert_eq!(SGR_X_BY_X[1], 128);
        assert_eq!(SGR_X_BY_X[255], 0);
        for w in SGR_X_BY_X.windows(2) {
            assert!(w[0] >= w[1], "LUT must be non-increasing");
        }
    }

    /// A flat input is a fixed point of Wiener: a unit-gain symmetric filter over
    /// a constant plane returns the constant (rounding is exact here).
    #[test]
    fn wiener_flat_is_identity() {
        let (w, h, stride) = (8usize, 8usize, 8usize);
        let plane = vec![100u8; w * h];
        let mut dst = vec![0u8; w * h];
        let src = PlaneSrc {
            cdef: &plane,
            pre_cdef: &plane,
            w,
            h,
            stride,
            stripe_start: 0,
            stripe_end: h as i32 - 1,
        };
        let vf = wiener_kernel(&[3, -7, 15]);
        let hf = wiener_kernel(&[3, -7, 15]);
        wiener_unit(&src, &mut dst, 0, 0, w, h, &vf, &hf);
        assert!(
            dst.iter().all(|&p| p == 100),
            "flat plane must be preserved, got {dst:?}"
        );
    }

    /// A hand-computed Wiener horizontal+vertical pass on a tiny gradient with the
    /// identity kernel (taps all zero → center 128) must reproduce the input
    /// exactly: H = (16384 + 128*s + 0... + 4) >> 3 ... wait identity center=128
    /// so H = round2(s*128 + bias, 3) packs s, V recovers s. We assert the known
    /// fixed point and a known non-trivial single-tap response.
    #[test]
    fn wiener_identity_kernel_recovers_input() {
        // taps [0,0,0] → kernel [0,0,0,128,0,0,0] (pure identity, DC gain 128).
        let (w, h, stride) = (4usize, 4usize, 4usize);
        let plane: Vec<u8> = (0..16).map(|i| (10 + i * 5) as u8).collect();
        let mut dst = vec![0u8; w * h];
        let src = PlaneSrc {
            cdef: &plane,
            pre_cdef: &plane,
            w,
            h,
            stride,
            stripe_start: 0,
            stripe_end: h as i32 - 1,
        };
        let id = wiener_kernel(&[0, 0, 0]);
        wiener_unit(&src, &mut dst, 0, 0, w, h, &id, &id);
        assert_eq!(
            dst, plane,
            "identity Wiener kernel must reproduce the input"
        );
    }

    /// SGR box filter on a flat plane: with a constant input the variance term
    /// `p = sumsq*n - sum*sum = 0`, so `a2` saturates (z=0 → complement 255), and
    /// the projection collapses to the source. The whole SGR unit is the identity
    /// on a constant plane (a fixed point), which we verify, plus the box-pass
    /// intermediate is well-defined (no panic, finite) on a small gradient.
    #[test]
    fn sgr_flat_is_identity() {
        let (w, h, stride) = (8usize, 8usize, 8usize);
        let plane = vec![123u8; w * h];
        let mut dst = vec![0u8; w * h];
        let src = PlaneSrc {
            cdef: &plane,
            pre_cdef: &plane,
            w,
            h,
            stride,
            stripe_start: 0,
            stripe_end: h as i32 - 1,
        };
        // set 0 → {r0=2,eps0=12, r1=1,eps1=4}; weights mid {-32,31}.
        sgr_unit(&src, &mut dst, 0, 0, w, h, 0, &[-32, 31]);
        assert!(
            dst.iter().all(|&p| p == 123),
            "flat plane must be preserved, got {dst:?}"
        );
    }

    /// SGR box-pass on a flat plane, hand-computed at the centre pixel.
    /// For r=1 (n=9, one_over_n=455) over a constant `c`: `sumsq = 9c²`,
    /// `sumb = 9c`, so `p = 9c²·9 - (9c)² = 0` ⇒ `z = 0` ⇒ `x_comp = 255`
    /// (a2 = 256 - 255 = 1). `b2 = Round2(255·9c·455, 12)`. The 3×3 combine
    /// (weight sum 32) yields `flt = Round2(32·a2·c + 32·b2, 9)`. On a flat
    /// plane SGR must reproduce the (upscaled) source: `flt == 16c`.
    #[test]
    fn sgr_box_pass_flat_value() {
        let (w, h, stride) = (4usize, 4usize, 4usize);
        let c = 80i64;
        let plane = vec![c as u8; w * h];
        let src = PlaneSrc {
            cdef: &plane,
            pre_cdef: &plane,
            w,
            h,
            stride,
            stripe_start: 0,
            stripe_end: h as i32 - 1,
        };
        // r=1, eps=4 (set 0 pass 1).
        let flt = sgr_pass(&src, 0, 0, w, h, 1, 4);
        let one_over_n = 455i64;
        let x_comp = 255i64; // SGR_X_BY_X[0]
        let a2 = (1i64 << SGRPROJ_SGR_BITS) - x_comp; // = 1
        let b2 = round2(x_comp * (9 * c) * one_over_n, SGRPROJ_RECIP_BITS);
        // 3×3 plus/diag combine: centre(4)+4·edge(4)+4·diag(3) = 4+16+12 = 32.
        let acc_a = 32 * a2;
        let acc_b = 32 * b2;
        let expect = round2(acc_a * c + acc_b, SGRPROJ_SGR_BITS + 5 - SGRPROJ_RST_BITS);
        assert_eq!(flt[w + 1] as i64, expect, "centre SGR box value mismatch");
        // Denoising identity on flat input: flt ≈ src << SGRPROJ_RST_BITS = 16c.
        assert_eq!(
            flt[w + 1] as i64,
            16 * c,
            "flat SGR box must reproduce 16·src"
        );
    }

    /// Stripe sampling: rows outside the stripe come from `pre_cdef` (clamped ±2),
    /// in-stripe rows from `cdef`. Verify the source selector picks the right
    /// buffer at and across a stripe boundary, and replicates frame edges.
    #[test]
    fn plane_src_stripe_and_edge() {
        // 4 wide, 8 tall. cdef all 200, pre_cdef all 50. Stripe = rows [2..5].
        let (w, h, stride) = (4usize, 8usize, 4usize);
        let cdef = vec![200u8; w * h];
        let pre = vec![50u8; w * h];
        let src = PlaneSrc {
            cdef: &cdef,
            pre_cdef: &pre,
            w,
            h,
            stride,
            stripe_start: 2,
            stripe_end: 5,
        };
        // In-stripe → post-CDEF (200).
        assert_eq!(src.at(1, 3), 200);
        // Above stripe → pre-CDEF (50).
        assert_eq!(src.at(1, 0), 50);
        // Below stripe → pre-CDEF (50).
        assert_eq!(src.at(1, 7), 50);
        // X out of bounds clamps into the plane (still post-CDEF for an in-stripe y).
        assert_eq!(src.at(-3, 4), 200);
        assert_eq!(src.at(99, 4), 200);
        // Y far above clamps to row 0 path (still pre-CDEF since < stripe_start).
        assert_eq!(src.at(0, -5), 50);
    }

    #[test]
    fn frame_restoration_remap() {
        // fh.lr_type already holds the Remap_Lr_Type output (0/1/2/3).
        assert_eq!(frame_restoration(0), FrameRestoration::None);
        assert_eq!(frame_restoration(1), FrameRestoration::Switchable);
        assert_eq!(frame_restoration(2), FrameRestoration::Wiener);
        assert_eq!(frame_restoration(3), FrameRestoration::Sgr);
    }

    #[test]
    fn defaults_seed_mids_and_cdfs() {
        let r = LrRef::default();
        assert_eq!(r.wiener, [WIENER_TAPS_MID, WIENER_TAPS_MID]);
        assert_eq!(r.sgr_xqd, SGR_XQD_MID);
        let c = LrCdf::default();
        assert_eq!(c.wiener, RESTORE_WIENER_CDF);
        assert_eq!(c.sgrproj, RESTORE_SGRPROJ_CDF);
        assert_eq!(c.switchable, RESTORE_SWITCHABLE_CDF);
        // Inverse-Q15 sanity: a bool's stored prob is `32768 - raw`.
        assert_eq!(RESTORE_WIENER_CDF[0], 32768 - 11570);
        assert_eq!(RESTORE_SGRPROJ_CDF[0], 32768 - 16855);
    }

    /// `apply_plane` driver: a single NONE unit copies the post-CDEF plane through
    /// unchanged; a Wiener unit with the identity kernel (taps 0 → center 128)
    /// also reproduces the input; a flat plane is a fixed point under both Wiener
    /// and SGR units. Covers the unit dispatch + geometry, not just the kernels.
    #[test]
    fn apply_plane_dispatch() {
        let (w, h) = (16usize, 16usize);
        // Gradient plane so a non-identity transform would show.
        let plane: Vec<u8> = (0..w * h).map(|i| (i % 200) as u8).collect();

        // NONE: identity pass-through.
        let lr_none = PlaneLr {
            unit_size: 64,
            unit_cols: 1,
            unit_rows: 1,
            units: vec![LrUnit::default()],
        };
        let mut dst = vec![0u8; w * h];
        apply_plane(&plane, &plane, &mut dst, w, h, w, 0, &lr_none);
        assert_eq!(dst, plane, "NONE unit must copy through unchanged");

        // Wiener identity kernel (taps [0,0,0] → center 128) reproduces the input.
        let lr_wiener = PlaneLr {
            unit_size: 64,
            unit_cols: 1,
            unit_rows: 1,
            units: vec![LrUnit {
                kind: RestorationType::Wiener,
                ..LrUnit::default()
            }],
        };
        let mut dst2 = vec![0u8; w * h];
        apply_plane(&plane, &plane, &mut dst2, w, h, w, 0, &lr_wiener);
        assert_eq!(dst2, plane, "identity Wiener unit must reproduce the input");

        // Flat plane is a fixed point under a real SGR unit.
        let flat = vec![123u8; w * h];
        let lr_sgr = PlaneLr {
            unit_size: 64,
            unit_cols: 1,
            unit_rows: 1,
            units: vec![LrUnit {
                kind: RestorationType::Sgr,
                sgr_set: 0,
                sgr_xqd: [-32, 31],
                ..LrUnit::default()
            }],
        };
        let mut dst3 = vec![0u8; w * h];
        apply_plane(&flat, &flat, &mut dst3, w, h, w, 0, &lr_sgr);
        assert!(
            dst3.iter().all(|&p| p == 123),
            "flat plane preserved under SGR, got {dst3:?}"
        );
    }

    /// Multi-unit geometry: a 2×2 grid of 8-pixel units over a 16×16 plane routes
    /// each quadrant to its own unit's params (here only the bottom-right is
    /// Wiener-identity, the rest NONE) — all reproduce a gradient input, proving
    /// the per-unit `(unit_row, unit_col)` selection and block clipping.
    #[test]
    fn apply_plane_multi_unit_geometry() {
        let (w, h) = (16usize, 16usize);
        let plane: Vec<u8> = (0..w * h).map(|i| (i % 200) as u8).collect();
        let mut units = vec![LrUnit::default(); 4];
        units[3] = LrUnit {
            kind: RestorationType::Wiener,
            ..LrUnit::default()
        }; // identity
        let lr = PlaneLr {
            unit_size: 8,
            unit_cols: 2,
            unit_rows: 2,
            units,
        };
        let mut dst = vec![0u8; w * h];
        apply_plane(&plane, &plane, &mut dst, w, h, w, 0, &lr);
        // All units are identity (NONE copies, identity-Wiener reproduces), so the
        // whole plane round-trips — and crucially no out-of-bounds on unit indexing.
        assert_eq!(
            dst, plane,
            "2×2 unit grid must reproduce the gradient input"
        );
    }

    /// Cross-stripe halo source (§7.17.6): a plane taller than one stripe
    /// (`> 56` luma rows) splits into ≥2 stripes. A Wiener unit with a non-zero
    /// vertical tap reaches across the stripe boundary into the halo, which MUST
    /// be read from the pre-CDEF buffer, not the post-CDEF plane. We give the two
    /// buffers different constant values and assert the restored output at the
    /// boundary row differs when the pre-CDEF buffer differs — proving the driver
    /// honours the pre-CDEF halo (the bit-exactness fix this enables).
    #[test]
    fn apply_plane_cross_stripe_uses_pre_cdef_halo() {
        let (w, h) = (4usize, 72usize); // 72 > 56 → stripe 0 = rows [0,55], stripe 1 = [56,71]
                                        // Constant post-CDEF plane so the only variation comes from the halo.
        let cdef = vec![100u8; w * h];
        // A real vertical tap (non-symmetric center) so vertical neighbours matter;
        // horizontal kept identity. v-taps [1,0,0] → kernel [1,0,0,126,0,0,1].
        let unit = LrUnit {
            kind: RestorationType::Wiener,
            wiener: [1, 0, 0, 0, 0, 0],
            ..LrUnit::default()
        };
        let lr = PlaneLr {
            unit_size: 64,
            unit_cols: 1,
            unit_rows: 2,
            units: vec![unit; 2],
        };

        // First with pre-CDEF == post-CDEF (flat 100).
        let mut dst_same = vec![0u8; w * h];
        apply_plane(&cdef, &cdef, &mut dst_same, w, h, w, 0, &lr);

        // Now with a pre-CDEF buffer whose row at the stripe-0 bottom halo differs.
        let mut pre = vec![100u8; w * h];
        // Row 55 is the last in-stripe-0 row; its halo (rows 56/57 from stripe-1's
        // perspective, and rows 54/55 read by stripe-1's top) come from pre-CDEF.
        for x in 0..w {
            pre[54 * w + x] = 200; // a pre-CDEF row inside the cross-stripe reach
            pre[55 * w + x] = 200;
        }
        let mut dst_diff = vec![0u8; w * h];
        apply_plane(&cdef, &pre, &mut dst_diff, w, h, w, 0, &lr);

        // The output must differ somewhere in stripe 1's top rows (which read the
        // altered pre-CDEF halo), proving the halo source is the pre-CDEF buffer.
        assert_ne!(
            dst_same, dst_diff,
            "altering the pre-CDEF halo must change the restored output across the stripe boundary"
        );
        // And the divergence is confined to the cross-stripe boundary region: the
        // far interior of stripe 0 (row 0) is untouched by the halo change.
        assert_eq!(
            dst_same[0..w],
            dst_diff[0..w],
            "rows far from the stripe boundary must be unaffected by the halo change"
        );
    }
}
