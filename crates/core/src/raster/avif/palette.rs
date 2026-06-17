//! AV1 palette mode (§5.11.46-50) — screen-content blocks whose pixels are an
//! index into a small per-block colour palette instead of a transform residual.
//! Transcribed from dav1d (`src/decode.c` `order_palette`/`read_pal_indices`,
//! `src/recon.c` `read_pal_plane`, BSD-2-Clause). Foundation layer: the pure
//! `order_palette` context/order builder + the index reader; the colour decode,
//! per-block neighbour state and reconstruction wire in next.

#![allow(dead_code)]

use super::msac::Msac;

/// Floor-log2 of a positive value — dav1d `ulog2` (`31 ^ clz`).
fn ulog2(x: u32) -> i32 {
    31 - x.leading_zeros() as i32
}

/// dav1d `read_pal_plane`: decode one plane's palette colours. `sz_cdf` is
/// `PAL_SZ[pl][sz_ctx]` (6-symbol adaptive CDF, size = symbol + 2). `l`/`a` are
/// the left / above neighbour palettes (colours, ascending) with sizes
/// `l_sz`/`a_sz` — the caller zeroes `a_sz` across the SB64 top boundary so the
/// above palette is not reused across superblock rows. `pl` is 0 for Y, 1 for U
/// (the `+!pl` bias on Y deltas keeps successive luma entries strictly rising).
pub(super) fn read_pal_plane(
    msac: &mut Msac,
    sz_cdf: &mut [u16],
    l: &[u8],
    l_sz: usize,
    a: &[u8],
    a_sz: usize,
    pl: usize,
) -> Vec<u8> {
    let pal_sz = msac.symbol_adapt(sz_cdf, 6) + 2;

    // Merge-sort + dedup the left & above palettes into the prediction cache.
    let mut cache = [0u8; 16];
    let mut n_cache = 0usize;
    macro_rules! push {
        ($v:expr) => {{
            let v = $v;
            if n_cache == 0 || cache[n_cache - 1] != v {
                cache[n_cache] = v;
                n_cache += 1;
            }
        }};
    }
    let (mut li, mut lc) = (0usize, l_sz);
    let (mut ai, mut ac) = (0usize, a_sz);
    while lc > 0 && ac > 0 {
        if l[li] < a[ai] {
            push!(l[li]);
            li += 1;
            lc -= 1;
        } else {
            if a[ai] == l[li] {
                li += 1;
                lc -= 1;
            }
            push!(a[ai]);
            ai += 1;
            ac -= 1;
        }
    }
    while lc > 0 {
        push!(l[li]);
        li += 1;
        lc -= 1;
    }
    while ac > 0 {
        push!(a[ai]);
        ai += 1;
        ac -= 1;
    }

    // Reused cache entries: one equi-bit each, until the palette is filled.
    let mut used = [0u8; 8];
    let mut nu = 0usize;
    let mut n = 0usize;
    while n < n_cache && nu < pal_sz {
        if msac.bool_equi() != 0 {
            used[nu] = cache[n];
            nu += 1;
        }
        n += 1;
    }

    // Freshly-coded entries: first literal, then delta-coded (adaptive width).
    let n_new = pal_sz - nu;
    let mut newp = Vec::with_capacity(n_new);
    if nu < pal_sz {
        let bpc = 8u32;
        let not_pl = (pl == 0) as i32;
        let max = (1i32 << bpc) - 1;
        let mut prev = msac.bools(bpc) as i32;
        newp.push(prev as u8);
        if newp.len() < n_new {
            let mut bits = bpc as i32 - 3 + msac.bools(2) as i32;
            loop {
                let delta = msac.bools(bits as u32) as i32;
                prev = (prev + delta + not_pl).min(max);
                newp.push(prev as u8);
                if prev + not_pl >= max {
                    while newp.len() < n_new {
                        newp.push(max as u8);
                    }
                    break;
                }
                bits = bits.min(1 + ulog2((max - prev - not_pl) as u32));
                if newp.len() >= n_new {
                    break;
                }
            }
        }
    }

    // Merge used-cache + new entries (both ascending) into the final palette.
    let mut pal = vec![0u8; pal_sz];
    let (mut nc, mut m) = (0usize, 0usize);
    for slot in pal.iter_mut() {
        if nc < nu && (m >= n_new || used[nc] <= newp[m]) {
            *slot = used[nc];
            nc += 1;
        } else {
            *slot = newp[m];
            m += 1;
        }
    }
    pal
}

/// dav1d `read_pal_uv`: U palette via `read_pal_plane(pl=1)`, then the V palette
/// (delta-coded with optional sign, or a literal run). Returns `(u_pal, v_pal)`.
pub(super) fn read_pal_uv(
    msac: &mut Msac,
    u_sz_cdf: &mut [u16],
    lu: &[u8],
    lu_sz: usize,
    au: &[u8],
    au_sz: usize,
) -> (Vec<u8>, Vec<u8>) {
    let u = read_pal_plane(msac, u_sz_cdf, lu, lu_sz, au, au_sz, 1);
    let pal_sz = u.len();
    let bpc = 8u32;
    let max = (1i32 << bpc) - 1;
    let mut v = vec![0u8; pal_sz];
    if msac.bool_equi() != 0 {
        let bits = bpc as i32 - 4 + msac.bools(2) as i32;
        let mut prev = msac.bools(bpc) as i32;
        v[0] = prev as u8;
        for vi in v.iter_mut().skip(1) {
            let mut delta = msac.bools(bits as u32) as i32;
            if delta != 0 && msac.bool_equi() != 0 {
                delta = -delta;
            }
            prev = (prev + delta) & max;
            *vi = prev as u8;
        }
    } else {
        for vi in v.iter_mut() {
            *vi = msac.bools(bpc) as u8;
        }
    }
    (u, v)
}

/// dav1d `read_pal_indices` (with `pal_idx_finish` collapsed into a direct
/// row-major fill). Decodes the per-pixel palette index map for one plane via
/// the anti-diagonal wavefront, then replicates the right/bottom edges for the
/// off-frame remainder. Returns a `bw*bh` row-major buffer (stride `bw`).
pub(super) fn read_pal_indices(
    msac: &mut Msac,
    color_map_cdf: &mut [[u16; 8]; 5],
    pal_sz: usize,
    bw: usize,
    bh: usize,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let stride = bw;
    let mut pal = vec![0u8; bw * bh];
    pal[0] = msac.decode_uniform(pal_sz as u32) as u8;
    let mut order = [[0u8; 8]; 8];
    let mut ctx = [0u8; 8];
    for i in 1..(w + h - 1) {
        // top/left → bottom/right diagonals ("wave-front").
        let first = i.min(w - 1);
        let last = i.saturating_sub(h - 1); // imax(0, i - h + 1)
        order_palette(&pal, stride, i, first, last, &mut order, &mut ctx);
        let mut m = 0usize;
        let mut j = first;
        loop {
            let cidx = msac.symbol_adapt(&mut color_map_cdf[ctx[m] as usize], pal_sz - 1);
            pal[(i - j) * stride + j] = order[m][cidx];
            m += 1;
            if j == last {
                break;
            }
            j -= 1;
        }
    }

    // `pal_idx_finish` edge-extension: replicate the last coded column, then the
    // last coded row, to fill the full block when it overhangs the frame edge.
    if w < bw {
        for y in 0..h {
            let last = pal[y * stride + w - 1];
            for x in w..bw {
                pal[y * stride + x] = last;
            }
        }
    }
    if h < bh {
        for y in h..bh {
            let (head, tail) = pal.split_at_mut(y * stride);
            tail[..bw].copy_from_slice(&head[(h - 1) * stride..(h - 1) * stride + bw]);
        }
    }
    pal
}

/// Build the per-pixel colour ordering + entropy context for one wavefront
/// (anti-diagonal) of a palette index block — dav1d `order_palette`. `pal_idx`
/// is the index grid (row stride `stride`), already filled for earlier
/// diagonals; the diagonal spans columns `j = first..=last` (`first >= last`).
/// For step `n` it writes `order[n]` (candidate colours, most-likely first, then
/// the remaining colours in value order) and `ctx[n]` (0..=4 from the 3 decoded
/// neighbours left/top/top-left).
pub(super) fn order_palette(
    pal_idx: &[u8],
    stride: usize,
    i: usize,
    first: usize,
    last: usize,
    order: &mut [[u8; 8]],
    ctx: &mut [u8],
) {
    let mut have_top = i > first;
    let s = stride as isize;
    let mut p = (first + (i - first) * stride) as isize;
    let mut n = 0usize;
    let mut j = first as isize;
    while j >= last as isize {
        let have_left = j > 0;
        // (have_left || have_top) is always true on a valid wavefront.
        let mut mask: u32 = 0;
        let mut o = 0usize;
        let row = &mut order[n];
        macro_rules! add {
            ($v:expr) => {{
                let v = $v;
                row[o] = v;
                o += 1;
                mask |= 1 << v;
            }};
        }
        if !have_left {
            ctx[n] = 0;
            add!(pal_idx[(p - s) as usize]);
        } else if !have_top {
            ctx[n] = 0;
            add!(pal_idx[(p - 1) as usize]);
        } else {
            let l = pal_idx[(p - 1) as usize] as i32;
            let t = pal_idx[(p - s) as usize] as i32;
            let tl = pal_idx[(p - s - 1) as usize] as i32;
            let same_t_l = t == l;
            let same_t_tl = t == tl;
            let same_l_tl = l == tl;
            if same_t_l && same_t_tl && same_l_tl {
                ctx[n] = 4;
                add!(t as u8);
            } else if same_t_l {
                ctx[n] = 3;
                add!(t as u8);
                add!(tl as u8);
            } else if same_t_tl || same_l_tl {
                ctx[n] = 2;
                add!(tl as u8);
                add!(if same_t_tl { l as u8 } else { t as u8 });
            } else {
                ctx[n] = 1;
                add!(t.min(l) as u8);
                add!(t.max(l) as u8);
                add!(tl as u8);
            }
        }
        // Fill the rest of the order with the colours not yet listed, in value
        // order (dav1d's `for m=1,bit=0; m<0x100; ...`).
        let mut m: u32 = 1;
        let mut bit: u8 = 0;
        while m < 0x100 {
            if mask & m == 0 {
                row[o] = bit;
                o += 1;
            }
            m <<= 1;
            bit += 1;
        }
        debug_assert_eq!(o, 8);
        have_top = true;
        j -= 1;
        n += 1;
        p += s - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_palette_contexts() {
        // 4×4 index grid (stride 4). Fill a known top row + left col, then probe
        // a diagonal where the neighbour relationships hit ctx 4 / 3 / 1.
        // grid:
        //   row0: 2 2 2 2
        //   row1: 2 . . .
        //   row2: 1 . . .
        let mut g = [0u8; 16];
        g[0] = 2;
        g[1] = 2;
        g[2] = 2;
        g[3] = 2;
        g[4] = 2;
        g[8] = 1;
        let mut order = [[0u8; 8]; 8];
        let mut ctx = [0u8; 8];
        // Diagonal i=2: covers (y,x) with y+x=2 → first=min(2,3)=2, last=max(0,2-4+1)=0.
        order_palette(&g, 4, 2, 2, 0, &mut order, &mut ctx);
        // n=0 → (y=0,x=2): have_top=false (i==first) → ctx 0, order[0][0]=top? no:
        // !have_top branch reads left = g[idx-1]. idx = first + 0 = 2 → g[1]=2.
        assert_eq!(ctx[0], 0);
        assert_eq!(order[0][0], 2);
        // n=1 → (y=1,x=1): l=g[4]=2, t=g[1]=2, tl=g[0]=2 → all same → ctx 4, color 2.
        assert_eq!(ctx[1], 4);
        assert_eq!(order[1][0], 2);
        // n=2 → (y=2,x=0): have_left=false → ctx 0, reads top = g[idx-stride].
        assert_eq!(ctx[2], 0);
        // Every order row is a permutation of 0..8 (all colours listed once).
        for row in order.iter().take(3) {
            let mut seen = [false; 8];
            for &v in row.iter() {
                assert!(!seen[v as usize], "duplicate colour in order");
                seen[v as usize] = true;
            }
        }
    }
}
