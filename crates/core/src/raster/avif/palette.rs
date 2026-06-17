//! AV1 palette mode (§5.11.46-50) — screen-content blocks whose pixels are an
//! index into a small per-block colour palette instead of a transform residual.
//! Transcribed from dav1d (`src/decode.c` `order_palette`/`read_pal_indices`,
//! `src/recon.c` `read_pal_plane`, BSD-2-Clause). Foundation layer: the pure
//! `order_palette` context/order builder + the index reader; the colour decode,
//! per-block neighbour state and reconstruction wire in next.

#![allow(dead_code)]

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
