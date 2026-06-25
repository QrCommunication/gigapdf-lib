//! JPEG 2000 dequantisation, inverse DWT and inverse multi-component transform
//! (ISO/IEC 15444-1 Annexes E, F, G). Pure `std`.
//!
//! Consumes a populated [`TileComponent`] (its code-blocks already gathered by
//! tier-2): runs tier-1 EBCOT ([`super::t1`]) on each code-block, scatters the
//! coefficients into per-subband buffers, dequantises (reversible = identity,
//! irreversible = scalar step), then performs the level-by-level inverse DWT
//! (5/3 reversible integer lifting or 9/7 irreversible float lifting). A
//! separate [`inverse_mct`] applies the inverse RCT/ICT across components.

use super::markers::Transform;
use super::packet::{Orientation, Subband, TileComponent};
use super::t1;
use crate::error::Result;

// 9/7 irreversible lifting constants (ISO/IEC 15444-1 Table F.4).
const ALPHA: f64 = -1.586_134_342_059_924;
const BETA: f64 = -0.052_980_118_572_961;
const GAMMA: f64 = 0.882_911_075_530_934;
const DELTA: f64 = 0.443_506_852_043_971;
const K: f64 = 1.230_174_104_914_001;

/// Reconstruct a tile-component's spatial samples from its decoded code-blocks.
pub fn reconstruct(tc: &TileComponent) -> Result<Vec<i32>> {
    let reversible = tc.cod.transform == Transform::Reversible;
    let guard = tc.quant.guard_bits;
    let roi_shift = tc.roi_shift as u32;

    // Decode every subband's coefficients into its own f64 buffer.
    // `bands[r]` holds the (HL, LH, HH) detail buffers for resolution r≥1, plus
    // the resolution-0 LL buffer in `ll`.
    let mut ll = Plane::zeros(0, 0);
    // Per-resolution detail planes (HL, LH, HH).
    let mut details: Vec<[Plane; 3]> = Vec::with_capacity(tc.resolutions.len().saturating_sub(1));

    for (r, res) in tc.resolutions.iter().enumerate() {
        if r == 0 {
            let sb = &res.subbands[0];
            ll = decode_subband(tc, sb, guard, roi_shift, reversible);
        } else {
            let mut trio = [Plane::zeros(0, 0), Plane::zeros(0, 0), Plane::zeros(0, 0)];
            for sb in &res.subbands {
                let plane = decode_subband(tc, sb, guard, roi_shift, reversible);
                let slot = match sb.orientation {
                    Orientation::Hl => 0,
                    Orientation::Lh => 1,
                    Orientation::Hh => 2,
                    Orientation::Ll => 0,
                };
                trio[slot] = plane;
            }
            details.push(trio);
        }
    }

    // Inverse DWT: start from the LL band (resolution 0) and synthesise each
    // higher resolution in turn.
    let mut cur = ll;
    for r in 1..tc.resolutions.len() {
        let res = &tc.resolutions[r];
        let [hl, lh, hh] = &details[r - 1];
        cur = idwt_level(&cur, hl, lh, hh, res.x0, res.y0, res.x1, res.y1, reversible);
    }

    // Round to integers (reversible lifting is already integer-valued; the
    // irreversible float path rounds to the nearest sample).
    let mut out = vec![0i32; tc.width * tc.height];
    for (o, &v) in out.iter_mut().zip(cur.data.iter()) {
        *o = v.round() as i32;
    }
    Ok(out)
}

/// A floating-point sample plane with an absolute (subband-grid) origin.
struct Plane {
    x0: u32,
    y0: u32,
    w: usize,
    h: usize,
    data: Vec<f64>,
}

impl Plane {
    fn zeros(w: usize, h: usize) -> Self {
        Plane {
            x0: 0,
            y0: 0,
            w,
            h,
            data: vec![0.0; w * h],
        }
    }

    fn at(&self, x: usize, y: usize) -> f64 {
        self.data[y * self.w + x]
    }
}

/// Decode one subband: tier-1 on each code-block, scatter into a plane and
/// dequantise.
fn decode_subband(
    tc: &TileComponent,
    sb: &Subband,
    guard: u32,
    roi_shift: u32,
    reversible: bool,
) -> Plane {
    let w = sb.width();
    let h = sb.height();
    let mut plane = Plane {
        x0: sb.x0,
        y0: sb.y0,
        w,
        h,
        data: vec![0.0; w * h],
    };
    if w == 0 || h == 0 {
        return plane;
    }

    // Maximum magnitude bit-planes for this subband: Mb = G + ε_b − 1.
    let exponent = sb.step.0;
    let mb = guard + exponent.max(1) - 1;
    // Dequant step (irreversible): Δ_b = 2^(Rb − ε_b) · (1 + μ_b/2^11),
    // Rb = bit_depth + gain_b.
    let rb = tc.bit_depth + sb.gain;
    let delta = if reversible {
        1.0
    } else {
        let mant = sb.step.1 as f64;
        2f64.powi(rb as i32 - exponent as i32) * (1.0 + mant / 2048.0)
    };
    // ROI background-to-foreground threshold (max-shift): coefficients with a
    // magnitude bit at or above this are region-of-interest and de-scaled.
    let roi_thresh = if roi_shift > 0 { 1u32 << mb } else { u32::MAX };

    for prec in &sb.precincts {
        for cb in &prec.blocks {
            if !cb.included || cb.num_passes == 0 || cb.data.is_empty() {
                continue;
            }
            let cbw = cb.width();
            let cbh = cb.height();
            let res = t1::decode_codeblock(
                &cb.data,
                cbw,
                cbh,
                mb,
                cb.zero_bit_planes,
                cb.num_passes,
                sb.orientation.t1_kind(),
                tc.cod.cb_style,
            );
            // Scatter the code-block into the subband plane at its offset.
            let off_x = (cb.x0 - sb.x0) as usize;
            let off_y = (cb.y0 - sb.y0) as usize;
            for cy in 0..cbh {
                for cx in 0..cbw {
                    let ci = cy * cbw + cx;
                    let mut mag = res.mag[ci];
                    if mag == 0 {
                        continue;
                    }
                    // tier-1 already OR-ed magnitude bits at their true plane, so
                    // `mag` is the integer magnitude at full bit weight. An ROI
                    // (max-shift) coefficient is scaled back down by the shift.
                    if roi_shift > 0 && mag >= roi_thresh {
                        mag >>= roi_shift;
                    }
                    let sign = if res.sign[ci] { -1.0 } else { 1.0 };
                    let value = if reversible {
                        sign * mag as f64
                    } else {
                        // Mid-point reconstruction at the lowest decoded plane.
                        let half = if res.lsb_shift > 0 {
                            (1u64 << res.lsb_shift) as f64 * 0.5
                        } else {
                            0.0
                        };
                        sign * (mag as f64 + half) * delta
                    };
                    let px = off_x + cx;
                    let py = off_y + cy;
                    if px < w && py < h {
                        plane.data[py * w + px] = value;
                    }
                }
            }
        }
    }
    plane
}

/// Synthesise resolution `r` from the reconstructed low band `ll` (resolution
/// r−1) and the three detail bands. Output spans absolute `[rx0,rx1)×[ry0,ry1)`.
#[allow(clippy::too_many_arguments)]
fn idwt_level(
    ll: &Plane,
    hl: &Plane,
    lh: &Plane,
    hh: &Plane,
    rx0: u32,
    ry0: u32,
    rx1: u32,
    ry1: u32,
    reversible: bool,
) -> Plane {
    let w = (rx1 - rx0) as usize;
    let h = (ry1 - ry0) as usize;
    let mut out = Plane {
        x0: rx0,
        y0: ry0,
        w,
        h,
        data: vec![0.0; w * h],
    };
    if w == 0 || h == 0 {
        return out;
    }

    // Low band column/row counts on the resolution-r grid.
    // Low (even) samples count `sn`, high (odd) `dn`, determined by the parity
    // of the resolution origin (`cas`).
    let cas_x = (rx0 & 1) as usize;
    let cas_y = (ry0 & 1) as usize;

    // 1) Build the interleaved 2D array: even rows/cols ← low band, odd ← high.
    // Vertical (column) first, then horizontal — order is interchangeable for a
    // separable transform; we follow column-then-row.
    // We assemble into `buf` of size w×h, then lift columns, then rows.
    let mut buf = vec![0.0f64; w * h];

    // Place coefficients. For a resolution-r sample at absolute (u,v):
    //   - even u, even v  → LL low-low  = ll
    //   - odd  u, even v  → HL (horizontal high) = hl
    //   - even u, odd  v  → LH (vertical high)   = lh
    //   - odd  u, odd  v  → HH                    = hh
    // The band's local index is the count of even/odd positions before u/v.
    for y in 0..h {
        let v = ry0 as usize + y;
        for x in 0..w {
            let u = rx0 as usize + x;
            buf[y * w + x] = sample_band(ll, hl, lh, hh, u, v);
        }
    }

    // 2) Inverse lifting along columns then rows.
    let mut col = vec![0.0f64; h];
    for x in 0..w {
        for y in 0..h {
            col[y] = buf[y * w + x];
        }
        idwt_1d(&mut col, cas_y, reversible);
        for y in 0..h {
            buf[y * w + x] = col[y];
        }
    }
    let mut row = vec![0.0f64; w];
    for y in 0..h {
        row[..w].copy_from_slice(&buf[y * w..y * w + w]);
        idwt_1d(&mut row, cas_x, reversible);
        buf[y * w..y * w + w].copy_from_slice(&row[..w]);
    }

    out.data = buf;
    out
}

/// Fetch the interleaved coefficient at absolute resolution-r position `(u,v)`
/// from the four bands (low-low, horizontal-high, vertical-high, high-high)
/// using each band's own absolute origin to find the local index.
fn sample_band(ll: &Plane, hl: &Plane, lh: &Plane, hh: &Plane, u: usize, v: usize) -> f64 {
    let (ux, uy) = (u >> 1, v >> 1);
    match (u & 1, v & 1) {
        (0, 0) => band_get(ll, ux, uy),
        (1, 0) => band_get(hl, ux, uy),
        (0, 1) => band_get(lh, ux, uy),
        _ => band_get(hh, ux, uy),
    }
}

/// Read a band sample by its absolute coordinate (`ax`,`ay`), mapping through
/// the band's origin. Out-of-range reads return 0 (boundary).
fn band_get(p: &Plane, ax: usize, ay: usize) -> f64 {
    if p.w == 0 || p.h == 0 {
        return 0.0;
    }
    let bx = ax as isize - p.x0 as isize;
    let by = ay as isize - p.y0 as isize;
    if bx < 0 || by < 0 || bx as usize >= p.w || by as usize >= p.h {
        return 0.0;
    }
    p.at(bx as usize, by as usize)
}

/// In-place 1D inverse DWT lifting on an interleaved line (`cas` = parity of the
/// first sample: 0 means index 0 is a low/even sample, 1 means high/odd).
fn idwt_1d(x: &mut [f64], cas: usize, reversible: bool) {
    let n = x.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        // A single sample is the DC (low) coefficient: 5/3 keeps it as-is; 9/7
        // undoes the analysis low-pass scaling (×K) by dividing by K.
        if !reversible {
            x[0] /= K;
        }
        return;
    }
    if reversible {
        idwt_53(x, cas);
    } else {
        idwt_97(x, cas);
    }
}

/// Symmetric (whole-sample) boundary extension index into `[0, n)`.
fn mirror(i: isize, n: usize) -> usize {
    let n = n as isize;
    if n == 1 {
        return 0;
    }
    let period = 2 * (n - 1);
    let mut k = i.rem_euclid(period);
    if k >= n {
        k = period - k;
    }
    k as usize
}

/// Inverse 5/3 reversible lifting (ISO/IEC 15444-1 F.3.6) on an interleaved
/// line. `cas`=0: even indices are low samples; `cas`=1: even indices are high.
fn idwt_53(x: &mut [f64], cas: usize) {
    let n = x.len();
    // Reorganise so that low samples sit at even indices (cas handling): we work
    // on absolute indices and treat `idx % 2 == cas_low` as low.
    // Step 1 (even/low update): s(n) -= floor((d(n-1)+d(n)+2)/4)
    // Step 2 (odd/high predict): d(n) += floor((s(n)+s(n+1))/2)
    let low_is_even = cas == 0;
    // Update step on low samples.
    for i in 0..n {
        let is_low = i.is_multiple_of(2) == low_is_even;
        if !is_low {
            continue;
        }
        let dl = high_neighbour(x, i as isize - 1, low_is_even, n);
        let dr = high_neighbour(x, i as isize + 1, low_is_even, n);
        let upd = ((dl + dr + 2.0) / 4.0).floor();
        x[i] -= upd;
    }
    // Predict step on high samples.
    for i in 0..n {
        let is_low = i.is_multiple_of(2) == low_is_even;
        if is_low {
            continue;
        }
        let sl = low_neighbour(x, i as isize - 1, low_is_even, n);
        let sr = low_neighbour(x, i as isize + 1, low_is_even, n);
        let pred = ((sl + sr) / 2.0).floor();
        x[i] += pred;
    }
}

/// Inverse 9/7 irreversible lifting (ISO/IEC 15444-1 F.3.7) on an interleaved
/// line, with the four lifting steps and the scaling constant `K`.
fn idwt_97(x: &mut [f64], cas: usize) {
    let n = x.len();
    let low_is_even = cas == 0;
    // Scaling: low samples ×(1/K), high samples ×K  (step 0, inverse).
    for (i, xi) in x.iter_mut().enumerate() {
        let is_low = i.is_multiple_of(2) == low_is_even;
        if is_low {
            *xi /= K;
        } else {
            *xi *= K;
        }
    }
    // Inverse step 4 (δ): low -= δ (high[-1]+high[+1])
    lift(x, n, low_is_even, true, -DELTA);
    // Inverse step 3 (γ): high -= γ (low[-1]+low[+1])
    lift(x, n, low_is_even, false, -GAMMA);
    // Inverse step 2 (β): low -= β (high[-1]+high[+1])
    lift(x, n, low_is_even, true, -BETA);
    // Inverse step 1 (α): high -= α (low[-1]+low[+1])
    lift(x, n, low_is_even, false, -ALPHA);
}

/// One 9/7 lifting step: add `coeff * (neighbour_left + neighbour_right)` to
/// every sample of the target parity. `target_low` selects low (true) or high
/// (false) samples; the neighbours are of the opposite parity.
fn lift(x: &mut [f64], n: usize, low_is_even: bool, target_low: bool, coeff: f64) {
    for i in 0..n {
        let is_low = i.is_multiple_of(2) == low_is_even;
        if is_low != target_low {
            continue;
        }
        let nl = opposite_neighbour(x, i as isize - 1, low_is_even, !target_low, n);
        let nr = opposite_neighbour(x, i as isize + 1, low_is_even, !target_low, n);
        x[i] += coeff * (nl + nr);
    }
}

/// Read the high-parity neighbour at index `i` (mirrored), used by 5/3 update.
fn high_neighbour(x: &[f64], i: isize, low_is_even: bool, n: usize) -> f64 {
    let m = mirror(i, n);
    let is_low = m.is_multiple_of(2) == low_is_even;
    if is_low {
        // Mirror landed on a low sample; reflect once more to the nearest high.
        let m2 = mirror(if i < 0 { i - 1 } else { i + 1 }, n);
        x[m2]
    } else {
        x[m]
    }
}

/// Read the low-parity neighbour at index `i` (mirrored), used by 5/3 predict.
fn low_neighbour(x: &[f64], i: isize, low_is_even: bool, n: usize) -> f64 {
    let m = mirror(i, n);
    let is_low = m.is_multiple_of(2) == low_is_even;
    if is_low {
        x[m]
    } else {
        let m2 = mirror(if i < 0 { i - 1 } else { i + 1 }, n);
        x[m2]
    }
}

/// Read a neighbour of a required parity at mirrored index `i` (9/7 lifting).
fn opposite_neighbour(x: &[f64], i: isize, low_is_even: bool, want_low: bool, n: usize) -> f64 {
    let m = mirror(i, n);
    let is_low = m.is_multiple_of(2) == low_is_even;
    if is_low == want_low {
        x[m]
    } else {
        let m2 = mirror(if i < 0 { i - 1 } else { i + 1 }, n);
        x[m2]
    }
}

/// Inverse multi-component transform across the first three component planes
/// (ISO/IEC 15444-1 Annex G). RCT for the reversible path, ICT for irreversible.
/// `spatial[c]` are the per-component reconstructed planes (component-grid).
pub fn inverse_mct(spatial: &mut [Vec<i32>], transform: Transform) {
    if spatial.len() < 3 {
        return;
    }
    // Disjoint mutable borrows of the first three component planes so the per-
    // pixel transform can read/write all three in one zipped pass.
    let (head, tail) = spatial.split_at_mut(1);
    let (mid, rest) = tail.split_at_mut(1);
    let c0 = &mut head[0];
    let c1 = &mut mid[0];
    let c2 = &mut rest[0];
    let n = c0.len().min(c1.len()).min(c2.len());
    let (c0, c1, c2) = (&mut c0[..n], &mut c1[..n], &mut c2[..n]);
    if transform == Transform::Reversible {
        for ((p0, p1), p2) in c0.iter_mut().zip(c1.iter_mut()).zip(c2.iter_mut()) {
            let (y, u, v) = (*p0, *p1, *p2); // Y, Db, Dr
            let g = y - ((u + v) >> 2);
            *p0 = v + g; // R
            *p1 = g; // G
            *p2 = u + g; // B
        }
    } else {
        for ((p0, p1), p2) in c0.iter_mut().zip(c1.iter_mut()).zip(c2.iter_mut()) {
            let (yy, cb, cr) = (*p0 as f64, *p1 as f64, *p2 as f64);
            *p0 = (yy + 1.402 * cr).round() as i32; // R
            *p1 = (yy - 0.344_136 * cb - 0.714_136 * cr).round() as i32; // G
            *p2 = (yy + 1.772 * cb).round() as i32; // B
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_reflects() {
        // n=4: indices … -1→1, 0,1,2,3, 4→2, 5→1
        assert_eq!(mirror(-1, 4), 1);
        assert_eq!(mirror(0, 4), 0);
        assert_eq!(mirror(3, 4), 3);
        assert_eq!(mirror(4, 4), 2);
        assert_eq!(mirror(5, 4), 1);
    }

    #[test]
    fn idwt_53_dc_is_preserved() {
        // A constant low band with zero high band must reconstruct a constant.
        // Interleaved layout, cas=0: x[0]=low, x[1]=high=0, x[2]=low, …
        let mut x = vec![10.0, 0.0, 10.0, 0.0, 10.0, 0.0];
        idwt_53(&mut x, 0);
        for &v in &x {
            assert!((v - 10.0).abs() < 1e-9, "got {v}");
        }
    }

    /// Forward 9/7 analysis (the exact inverse of [`idwt_97`]) — test-only, used
    /// to validate the synthesis by round-trip. Reverses the synthesis: the
    /// lifting steps run in the opposite order with the opposite sign, then the
    /// scaling is inverted (low ×K, high ×1/K).
    fn fwd_97(x: &mut [f64], cas: usize) {
        let n = x.len();
        let low_is_even = cas == 0;
        // Reverse of synthesis step α, then γ, then β-as-step2, then δ.
        lift(x, n, low_is_even, false, ALPHA); // high += α(low+low)
        lift(x, n, low_is_even, true, BETA); // low  += β(high+high)
        lift(x, n, low_is_even, false, GAMMA); // high += γ(low+low)
        lift(x, n, low_is_even, true, DELTA); // low  += δ(high+high)
        for (i, xi) in x.iter_mut().enumerate() {
            let is_low = i.is_multiple_of(2) == low_is_even;
            if is_low {
                *xi *= K;
            } else {
                *xi /= K;
            }
        }
    }

    #[test]
    fn idwt_97_inverts_forward() {
        // A non-trivial signal: forward 9/7 then inverse must recover it.
        let orig = [12.0, 4.0, 9.0, 17.0, 3.0, 8.0, 14.0, 6.0];
        let mut x = orig;
        fwd_97(&mut x, 0);
        idwt_97(&mut x, 0);
        for (a, b) in x.iter().zip(orig.iter()) {
            assert!((a - b).abs() < 1e-6, "got {a}, want {b}");
        }
    }

    #[test]
    fn idwt_97_dc_roundtrip() {
        // A constant signal survives a forward+inverse 9/7 round-trip.
        let c = 8.0;
        let orig = [c; 6];
        let mut x = orig;
        fwd_97(&mut x, 0);
        idwt_97(&mut x, 0);
        for &v in &x {
            assert!((v - c).abs() < 1e-6, "got {v}");
        }
    }

    #[test]
    fn inverse_rct_roundtrip() {
        // Forward RCT of (R,G,B) then inverse must recover (R,G,B).
        let (r, g, b) = (200i32, 100, 50);
        let yy = (r + 2 * g + b) >> 2;
        let db = b - g;
        let dr = r - g;
        let mut planes = vec![vec![yy], vec![db], vec![dr]];
        inverse_mct(&mut planes, Transform::Reversible);
        assert_eq!(planes[0][0], r);
        assert_eq!(planes[1][0], g);
        assert_eq!(planes[2][0], b);
    }
}
