// Intra predictors + border fixups, translated from RFC 6386 predict.c.
// Included into decoder.rs. All functions take a plane `buf`, a base index `p`
// (top-left of the block / subblock) and the row stride `s`. Neighbour pixels
// (`above` = p-s, `left` = p-1, corner = p-s-1) live in the BORDER guard region.

#[inline]
fn avg3(a: i32, b: i32, c: i32) -> u8 {
    ((a + 2 * b + c + 2) >> 2) as u8
}
#[inline]
fn avg2(a: i32, b: i32) -> u8 {
    ((a + b + 1) >> 1) as u8
}

fn predict_block(buf: &mut [u8], p: usize, s: usize, n: usize, mode: i32) {
    match mode {
        x if x == V_PRED => {
            for i in 0..n {
                for j in 0..n {
                    buf[p + i * s + j] = buf[p - s + j];
                }
            }
        }
        x if x == H_PRED => {
            for i in 0..n {
                let l = buf[p + i * s - 1];
                for j in 0..n {
                    buf[p + i * s + j] = l;
                }
            }
        }
        x if x == TM_PRED => {
            let corner = buf[p - s - 1] as i32;
            for i in 0..n {
                let l = buf[p + i * s - 1] as i32;
                for j in 0..n {
                    let a = buf[p - s + j] as i32;
                    buf[p + i * s + j] = clamp255(l + a - corner);
                }
            }
        }
        _ => {
            // DC_PRED
            let mut dc = 0i32;
            for i in 0..n {
                dc += buf[p + i * s - 1] as i32 + buf[p - s + i] as i32;
            }
            let shift = match n {
                16 => 5,
                8 => 4,
                _ => 3,
            };
            let dcv = ((dc + (1 << (shift - 1))) >> shift) as u8;
            for i in 0..n {
                for j in 0..n {
                    buf[p + i * s + j] = dcv;
                }
            }
        }
    }
}

fn fixup_left(buf: &mut [u8], p: usize, s: usize, width: usize, row: usize, mode: i32) {
    if mode == DC_PRED && row > 0 {
        for i in 0..width {
            buf[p + i * s - 1] = buf[p - s + i];
        }
    } else {
        // left column incl. above-left corner = 129
        let start = p - 1 - s;
        for i in 0..=width {
            buf[start + i * s] = 129;
        }
    }
}

fn fixup_above(buf: &mut [u8], p: usize, s: usize, width: usize, col: usize, mode: i32) {
    if mode == DC_PRED && col > 0 {
        for i in 0..width {
            buf[p - s + i] = buf[p - 1 + i * s];
        }
    } else {
        // above row incl. above-left corner = 127
        for i in 0..=width {
            buf[p - s - 1 + i] = 127;
        }
    }
    // above-right 4px = 127 (for 4×4 above-right modes)
    for i in 0..4 {
        buf[p - s + width + i] = 127;
    }
}

// ── 4×4 sub-block predictors ──────────────────────────────────────────────────
// `a(k)` reads above[k] (k may be -1..7), `l(k)` reads left[k] (0..3),
// `c` is the above-left corner.

fn pred4_dc(buf: &mut [u8], p: usize, s: usize) {
    predict_block(buf, p, s, 4, DC_PRED);
}
fn pred4_tm(buf: &mut [u8], p: usize, s: usize) {
    predict_block(buf, p, s, 4, TM_PRED);
}

fn pred4_ve(buf: &mut [u8], p: usize, s: usize) {
    let a = |k: isize| buf[(p as isize - s as isize + k) as usize] as i32;
    let r = [
        avg3(a(-1), a(0), a(1)),
        avg3(a(0), a(1), a(2)),
        avg3(a(1), a(2), a(3)),
        avg3(a(2), a(3), a(4)),
    ];
    for i in 0..4 {
        for j in 0..4 {
            buf[p + i * s + j] = r[j];
        }
    }
}

fn pred4_he(buf: &mut [u8], p: usize, s: usize) {
    let lc = |k: isize| buf[(p as isize + k * s as isize - 1) as usize] as i32;
    let corner = buf[p - s - 1] as i32;
    let v0 = avg3(corner, lc(0), lc(1));
    let v1 = avg3(lc(0), lc(1), lc(2));
    let v2 = avg3(lc(1), lc(2), lc(3));
    let v3 = avg3(lc(2), lc(3), lc(3));
    let rows = [v0, v1, v2, v3];
    for (i, &val) in rows.iter().enumerate() {
        for j in 0..4 {
            buf[p + i * s + j] = val;
        }
    }
}

fn pred4_ld(buf: &mut [u8], p: usize, s: usize) {
    let a = |k: usize| buf[p - s + k] as i32;
    let pr = [
        avg3(a(0), a(1), a(2)),
        avg3(a(1), a(2), a(3)),
        avg3(a(2), a(3), a(4)),
        avg3(a(3), a(4), a(5)),
        avg3(a(4), a(5), a(6)),
        avg3(a(5), a(6), a(7)),
        avg3(a(6), a(7), a(7)),
    ];
    for r in 0..4 {
        for c in 0..4 {
            buf[p + r * s + c] = pr[r + c];
        }
    }
}

fn pred4_rd(buf: &mut [u8], p: usize, s: usize) {
    let a = |k: isize| buf[(p as isize - s as isize + k) as usize] as i32;
    let l = |k: isize| buf[(p as isize + k * s as isize - 1) as usize] as i32;
    let e0 = avg3(l(3), l(2), l(1));
    let e1 = avg3(l(2), l(1), l(0));
    let e2 = avg3(l(1), l(0), a(-1));
    let e3 = avg3(l(0), a(-1), a(0));
    let e4 = avg3(a(-1), a(0), a(1));
    let e5 = avg3(a(0), a(1), a(2));
    let e6 = avg3(a(1), a(2), a(3));
    // diagonal: row r, col c → e[3 - r + c]
    let d = [e0, e1, e2, e3, e4, e5, e6];
    for r in 0..4 {
        for c in 0..4 {
            buf[p + r * s + c] = d[3 - r + c];
        }
    }
}

fn pred4_vr(buf: &mut [u8], p: usize, s: usize) {
    let a = |k: isize| buf[(p as isize - s as isize + k) as usize] as i32;
    let l = |k: isize| buf[(p as isize + k * s as isize - 1) as usize] as i32;
    let p0 = avg2(a(-1), a(0));
    let p1 = avg2(a(0), a(1));
    let p2 = avg2(a(1), a(2));
    let p3 = avg2(a(2), a(3));
    let p4 = avg3(l(0), a(-1), a(0));
    let p5 = avg3(a(-1), a(0), a(1));
    let p6 = avg3(a(0), a(1), a(2));
    let p7 = avg3(a(1), a(2), a(3));
    let p8 = avg3(l(1), l(0), a(-1));
    let p9 = avg3(l(2), l(1), l(0));
    let g = [
        [p0, p1, p2, p3],
        [p4, p5, p6, p7],
        [p8, p0, p1, p2],
        [p9, p4, p5, p6],
    ];
    write4(buf, p, s, &g);
}

fn pred4_vl(buf: &mut [u8], p: usize, s: usize) {
    let a = |k: usize| buf[p - s + k] as i32;
    let p0 = avg2(a(0), a(1));
    let p1 = avg2(a(1), a(2));
    let p2 = avg2(a(2), a(3));
    let p3 = avg2(a(3), a(4));
    let p4 = avg3(a(0), a(1), a(2));
    let p5 = avg3(a(1), a(2), a(3));
    let p6 = avg3(a(2), a(3), a(4));
    let p7 = avg3(a(3), a(4), a(5));
    let p8 = avg3(a(4), a(5), a(6));
    let p9 = avg3(a(5), a(6), a(7));
    let g = [
        [p0, p1, p2, p3],
        [p4, p5, p6, p7],
        [p1, p2, p3, p8],
        [p5, p6, p7, p9],
    ];
    write4(buf, p, s, &g);
}

fn pred4_hd(buf: &mut [u8], p: usize, s: usize) {
    let a = |k: isize| buf[(p as isize - s as isize + k) as usize] as i32;
    let l = |k: isize| buf[(p as isize + k * s as isize - 1) as usize] as i32;
    let p0 = avg2(l(0), a(-1));
    let p1 = avg3(l(0), a(-1), a(0));
    let p2 = avg3(a(-1), a(0), a(1));
    let p3 = avg3(a(0), a(1), a(2));
    let p4 = avg2(l(1), l(0));
    let p5 = avg3(l(1), l(0), a(-1));
    let p6 = avg2(l(2), l(1));
    let p7 = avg3(l(2), l(1), l(0));
    let p8 = avg2(l(3), l(2));
    let p9 = avg3(l(3), l(2), l(1));
    let g = [
        [p0, p1, p2, p3],
        [p4, p5, p0, p1],
        [p6, p7, p4, p5],
        [p8, p9, p6, p7],
    ];
    write4(buf, p, s, &g);
}

fn pred4_hu(buf: &mut [u8], p: usize, s: usize) {
    let l = |k: usize| buf[p + k * s - 1] as i32;
    let p0 = avg2(l(0), l(1));
    let p1 = avg3(l(0), l(1), l(2));
    let p2 = avg2(l(1), l(2));
    let p3 = avg3(l(1), l(2), l(3));
    let p4 = avg2(l(2), l(3));
    let p5 = avg3(l(2), l(3), l(3));
    let p6 = l(3) as u8;
    let g = [
        [p0, p1, p2, p3],
        [p2, p3, p4, p5],
        [p4, p5, p6, p6],
        [p6, p6, p6, p6],
    ];
    write4(buf, p, s, &g);
}

#[inline]
fn write4(buf: &mut [u8], p: usize, s: usize, g: &[[u8; 4]; 4]) {
    for (r, row) in g.iter().enumerate() {
        for (c, &val) in row.iter().enumerate() {
            buf[p + r * s + c] = val;
        }
    }
}

fn b_pred(buf: &mut [u8], p: usize, s: usize, modes: &[u8; 16], coeffs: &[i32], _last_col: bool) {
    // copy_down: replicate the MB above-right 4px down to subblock rows 4/8/12
    for k in 0..4 {
        let src = buf[p - s + 16 + k];
        buf[p + 3 * s + 16 + k] = src;
        buf[p + 7 * s + 16 + k] = src;
        buf[p + 11 * s + 16 + k] = src;
    }
    for i in 0..16 {
        let bp = p + (i >> 2) * 4 * s + (i & 3) * 4;
        match modes[i] as i32 {
            x if x == B_DC_PRED => pred4_dc(buf, bp, s),
            x if x == B_TM_PRED => pred4_tm(buf, bp, s),
            x if x == B_VE_PRED => pred4_ve(buf, bp, s),
            x if x == B_HE_PRED => pred4_he(buf, bp, s),
            x if x == B_LD_PRED => pred4_ld(buf, bp, s),
            x if x == B_RD_PRED => pred4_rd(buf, bp, s),
            x if x == B_VR_PRED => pred4_vr(buf, bp, s),
            x if x == B_VL_PRED => pred4_vl(buf, bp, s),
            x if x == B_HD_PRED => pred4_hd(buf, bp, s),
            _ => pred4_hu(buf, bp, s),
        }
        idct_add(buf, bp, s, &coeffs[i * 16..i * 16 + 16]);
    }
}
