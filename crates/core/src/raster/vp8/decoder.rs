//! VP8 keyframe decode pipeline — a faithful translation of the RFC 6386
//! reference decoder ("dixie", BSD-licensed). Intra-only (WebP still images are a
//! single keyframe). Produces YUV420, then converts to RGBA.
//!
//! The loop filter (RFC §15) is applied as a separable post-process; see
//! `loop_filter`.

use super::{
    Bool, AC_QUANT, BMODE_TREE, B_DC_PRED, B_HD_PRED, B_HE_PRED, B_LD_PRED, B_PRED, B_RD_PRED,
    B_TM_PRED, B_VE_PRED, B_VL_PRED, B_VR_PRED, COEFF_BANDS, COEFF_UPDATE_PROBS, DC_PRED, DC_QUANT,
    DEFAULT_COEFF_PROBS, H_PRED, KF_BMODE_PROBS, KF_UV_MODE_PROB, KF_UV_MODE_TREE, KF_YMODE_PROB,
    KF_YMODE_TREE, TM_PRED, V_PRED, ZIGZAG,
};

/// Border pixels around each plane (matches dixie's VP8BORDERINPIXELS).
const BORDER: usize = 32;

// ── extra-bit categories (dixie order: probs[bit], bit high→low) ──────────────
const CAT_BASE: [i32; 6] = [5, 7, 11, 19, 35, 67];
const CAT_PROBS: [&[u8]; 6] = [
    &[159],
    &[145, 165],
    &[140, 148, 173],
    &[135, 140, 155, 176],
    &[130, 134, 141, 157, 180],
    &[129, 130, 133, 140, 153, 177, 196, 230, 243, 254, 254],
];

// Per-block token entropy context: maps coefficient block index 0..25 to one of
// 9 left/above context slots (dixie left_context_index / above_context_index).
const LEFT_CTX: [usize; 25] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8,
];
const ABOVE_CTX: [usize; 25] = [
    0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 4, 5, 4, 5, 6, 7, 6, 7, 8,
];

#[inline]
fn maybe_int(b: &mut Bool, n: u32) -> i32 {
    if b.flag() {
        b.signed(n)
    } else {
        0
    }
}

#[inline]
fn clamp255(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ── dequant ───────────────────────────────────────────────────────────────────
#[inline]
fn dc_q(q: i32) -> i32 {
    DC_QUANT[q.clamp(0, 127) as usize]
}
#[inline]
fn ac_q(q: i32) -> i32 {
    AC_QUANT[q.clamp(0, 127) as usize]
}

#[derive(Clone, Copy, Default)]
struct Dequant {
    y1: [i32; 2],
    y2: [i32; 2],
    uv: [i32; 2],
}

#[derive(Clone, Copy, Default)]
struct Mb {
    y_mode: i32,
    uv_mode: i32,
    b_modes: [u8; 16],
    skip: bool,
    segment: usize,
    eob: bool, // any non-zero coefficient (gates sub-block loop filtering)
}

/// Loop-filter parameters parsed from the frame header.
#[derive(Default)]
struct Lf {
    level: i32,
    sharpness: i32,
    use_simple: bool,
    delta_enabled: bool,
    ref_delta: [i32; 4],
    mode_delta: [i32; 4],
    seg_enabled: bool,
    seg_abs: bool,
    seg_lf: [i32; 4],
}

/// Decode a VP8 keyframe chunk body to `(width, height, rgba)`.
pub fn decode_keyframe(body: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let p = decode_to_planes(body)?;
    let rgba = yuv_to_rgba(&p);
    Some((p.w as u32, p.h as u32, rgba))
}

struct Planes {
    w: usize,
    h: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    ys: usize, // luma stride
    cs: usize, // chroma stride
    y0: usize, // index of luma pixel (0,0)
    c0: usize, // index of chroma pixel (0,0)
}

fn decode_to_planes(body: &[u8]) -> Option<Planes> {
    // ── frame tag (3 bytes LE) ──
    if body.len() < 10 {
        return None;
    }
    let raw = body[0] as u32 | (body[1] as u32) << 8 | (body[2] as u32) << 16;
    let keyframe = (raw & 1) == 0;
    if !keyframe {
        return None; // inter frames out of scope
    }
    let part0_sz = (raw >> 5) as usize & 0x7FFFF;
    // keyframe header: sync + dims
    if body[3] != 0x9d || body[4] != 0x01 || body[5] != 0x2a {
        return None;
    }
    let raw2 =
        body[6] as u32 | (body[7] as u32) << 8 | (body[8] as u32) << 16 | (body[9] as u32) << 24;
    let w = (raw2 & 0x3FFF) as usize;
    let h = ((raw2 >> 16) & 0x3FFF) as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let mb_cols = w.div_ceil(16);
    let mb_rows = h.div_ceil(16);

    let data = &body[10..];
    if data.len() < part0_sz {
        return None;
    }
    let mut bd = Bool::new(&data[..part0_sz]);

    // ── header ──
    bd.literal(2); // colorspace + clamp (must be 0 for keyframe)

    // segmentation header
    let mut seg_enabled = false;
    let mut seg_update_map = false;
    let mut seg_abs = false;
    let mut seg_quant = [0i32; 4];
    let mut seg_lf = [0i32; 4];
    let mut seg_tree_probs = [255u8; 3];
    if bd.flag() {
        seg_enabled = true;
        seg_update_map = bd.flag();
        let update_data = bd.flag();
        if update_data {
            seg_abs = bd.flag();
            for q in seg_quant.iter_mut() {
                *q = maybe_int(&mut bd, 7);
            }
            for lf in seg_lf.iter_mut() {
                *lf = maybe_int(&mut bd, 6); // lf_level per segment (loop filter)
            }
        }
        if seg_update_map {
            for tp in seg_tree_probs.iter_mut() {
                *tp = if bd.flag() { bd.literal(8) as u8 } else { 255 };
            }
        }
    }

    // loop filter header
    let mut lf = Lf {
        use_simple: bd.flag(),
        level: bd.literal(6) as i32,
        sharpness: bd.literal(3) as i32,
        delta_enabled: bd.flag(),
        seg_enabled,
        seg_abs,
        seg_lf,
        ..Lf::default()
    };
    if lf.delta_enabled && bd.flag() {
        for d in lf.ref_delta.iter_mut() {
            *d = maybe_int(&mut bd, 6);
        }
        for d in lf.mode_delta.iter_mut() {
            *d = maybe_int(&mut bd, 6);
        }
    }

    // token partitions (live in the data AFTER part0)
    let token_data = &data[part0_sz..];
    let num_parts = 1usize << bd.literal(2);
    let mut partitions: Vec<Bool> = Vec::with_capacity(num_parts);
    {
        let sizes_len = 3 * (num_parts - 1);
        if token_data.len() < sizes_len {
            return None;
        }
        let mut off = sizes_len;
        let mut remaining = token_data.len() - sizes_len;
        for i in 0..num_parts {
            let psz = if i < num_parts - 1 {
                let b = &token_data[i * 3..];
                (b[0] as usize) | (b[1] as usize) << 8 | (b[2] as usize) << 16
            } else {
                remaining
            };
            if remaining < psz {
                return None;
            }
            partitions.push(Bool::new(&token_data[off..off + psz]));
            off += psz;
            remaining -= psz;
        }
    }

    // quantizer header
    let q_index = bd.literal(7) as i32;
    let y1_dc_dq = maybe_int(&mut bd, 4);
    let y2_dc_dq = maybe_int(&mut bd, 4);
    let y2_ac_dq = maybe_int(&mut bd, 4);
    let uv_dc_dq = maybe_int(&mut bd, 4);
    let uv_ac_dq = maybe_int(&mut bd, 4);

    // reference header (keyframe: only refresh_entropy matters)
    let _refresh_entropy = bd.flag();

    // entropy header: coeff prob updates over the keyframe defaults
    let mut coeff_probs = DEFAULT_COEFF_PROBS;
    for i in 0..4 {
        for j in 0..8 {
            for k in 0..3 {
                for l in 0..11 {
                    if bd.get(COEFF_UPDATE_PROBS[i][j][k][l]) != 0 {
                        coeff_probs[i][j][k][l] = bd.literal(8) as u8;
                    }
                }
            }
        }
    }
    let coeff_skip_enabled = bd.flag();
    let coeff_skip_prob = if coeff_skip_enabled {
        bd.literal(8) as u8
    } else {
        0
    };

    // dequant factors per segment
    let n_seg = if seg_enabled { 4 } else { 1 };
    let mut dq = [Dequant::default(); 4];
    for (i, d) in dq.iter_mut().enumerate().take(n_seg) {
        let mut q = q_index;
        if seg_enabled {
            q = if seg_abs {
                seg_quant[i]
            } else {
                q + seg_quant[i]
            };
        }
        d.y1[0] = dc_q(q + y1_dc_dq);
        d.y1[1] = ac_q(q);
        d.uv[0] = dc_q(q + uv_dc_dq).min(132);
        d.uv[1] = ac_q(q + uv_ac_dq);
        d.y2[0] = dc_q(q + y2_dc_dq) * 2;
        d.y2[1] = (ac_q(q + y2_ac_dq) * 155 / 100).max(8);
    }

    // ── planes (I420 with border) ──
    let ys = mb_cols * 16 + 2 * BORDER;
    let cs = mb_cols * 8 + 2 * BORDER;
    let yh = mb_rows * 16 + 2 * BORDER;
    let ch = mb_rows * 8 + 2 * BORDER;
    let mut pl = Planes {
        w,
        h,
        y: vec![0u8; ys * yh],
        u: vec![0u8; cs * ch],
        v: vec![0u8; cs * ch],
        ys,
        cs,
        y0: BORDER * ys + BORDER,
        c0: BORDER * cs + BORDER,
    };

    // per-MB info + token entropy contexts
    let mut mbinfo = vec![Mb::default(); mb_rows * mb_cols];
    let mut above_y = vec![DC_PRED as u8; mb_cols * 4]; // above sub-block modes (for b_pred ctx)
    let mut above_ctx = vec![[0u8; 9]; mb_cols]; // token above context per column
    let mut coeffs = vec![0i32; 25 * 16];

    for row in 0..mb_rows {
        // modemv: read modes for the whole row
        let mut left_modes = [DC_PRED as u8; 4];
        for col in 0..mb_cols {
            let mut mb = Mb::default();
            if seg_update_map {
                mb.segment = read_segment_id(&mut bd, &seg_tree_probs);
            }
            if coeff_skip_enabled {
                mb.skip = bd.get(coeff_skip_prob) != 0;
            }
            read_kf_modes(&mut bd, &mut mb, &mut left_modes, &mut above_y, col);
            mbinfo[row * mb_cols + col] = mb;
        }

        // tokens: decode coefficients per MB, then predict + reconstruct
        let part = &mut partitions[row % num_parts];
        let mut left_ctx = [0u8; 9];
        for col in 0..mb_cols {
            let mb = mbinfo[row * mb_cols + col];
            for c in coeffs.iter_mut() {
                *c = 0;
            }
            let eob = if mb.skip {
                reset_mb_ctx(&mut left_ctx, &mut above_ctx[col], mb.y_mode);
                false
            } else {
                decode_mb_tokens(
                    part,
                    &mut left_ctx,
                    &mut above_ctx[col],
                    &mut coeffs,
                    mb.y_mode,
                    &coeff_probs,
                    &dq[mb.segment],
                )
            };
            mbinfo[row * mb_cols + col].eob = eob;
            reconstruct_mb(&mut pl, row, col, &mb, &mut coeffs, mb_cols);
        }
    }

    // loop filter (separable post-process)
    loop_filter(&mut pl, &mbinfo, mb_rows, mb_cols, &lf);

    Some(pl)
}

// ── modes ─────────────────────────────────────────────────────────────────────

fn read_segment_id(b: &mut Bool, probs: &[u8; 3]) -> usize {
    if b.get(probs[0]) != 0 {
        2 + (b.get(probs[2]) != 0) as usize
    } else {
        (b.get(probs[1]) != 0) as usize
    }
}

fn read_kf_modes(b: &mut Bool, mb: &mut Mb, left: &mut [u8; 4], above: &mut [u8], col: usize) {
    let y_mode = b.tree(&KF_YMODE_TREE, &KF_YMODE_PROB);
    mb.y_mode = y_mode;
    let above_b = &mut above[col * 4..col * 4 + 4];
    if y_mode == B_PRED {
        for i in 0..16 {
            // above sub-block mode
            let a = if i < 4 { above_b[i] } else { mb.b_modes[i - 4] };
            // left sub-block mode
            let l = if i & 3 == 0 {
                left[i >> 2]
            } else {
                mb.b_modes[i - 1]
            };
            let m = b.tree(&BMODE_TREE, &KF_BMODE_PROBS[a as usize][l as usize]);
            mb.b_modes[i] = m as u8;
        }
        // export bottom row (above) and right col (left) for neighbours
        for k in 0..4 {
            above_b[k] = mb.b_modes[12 + k];
            left[k] = mb.b_modes[k * 4 + 3];
        }
    } else {
        // map whole-MB mode to the equivalent B sub-mode for neighbour context
        let bm = match y_mode {
            DC_PRED => B_DC_PRED,
            V_PRED => B_VE_PRED,
            H_PRED => B_HE_PRED,
            _ => B_TM_PRED,
        } as u8;
        for k in 0..4 {
            above_b[k] = bm;
            left[k] = bm;
        }
    }
    mb.uv_mode = b.tree(&KF_UV_MODE_TREE, &KF_UV_MODE_PROB);
}

// ── token decode ────────────────────────────────────────────────────────────

fn reset_mb_ctx(left: &mut [u8; 9], above: &mut [u8; 9], y_mode: i32) {
    for i in 0..8 {
        left[i] = 0;
        above[i] = 0;
    }
    if y_mode != B_PRED {
        left[8] = 0;
        above[8] = 0;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_mb_tokens(
    b: &mut Bool,
    left: &mut [u8; 9],
    above: &mut [u8; 9],
    coeffs: &mut [i32],
    y_mode: i32,
    probs: &[[[[u8; 11]; 3]; 8]; 4],
    dq: &Dequant,
) -> bool {
    // block iteration order. Type: 0=Y(after Y2), 1=Y2, 2=UV, 3=Y(no Y2).
    let has_y2 = y_mode != B_PRED;
    // (block_index, type, dc_factor, ac_factor, first_coeff)
    let mut order: Vec<(usize, usize)> = Vec::with_capacity(25);
    if has_y2 {
        order.push((24, 1));
        for i in 0..16 {
            order.push((i, 0));
        }
    } else {
        for i in 0..16 {
            order.push((i, 3));
        }
    }
    for i in 16..24 {
        order.push((i, 2));
    }

    let mut any = false;
    for &(blk, typ) in &order {
        let (dc, ac) = match typ {
            1 => (dq.y2[0], dq.y2[1]),
            2 => (dq.uv[0], dq.uv[1]),
            _ => (dq.y1[0], dq.y1[1]),
        };
        let first = if typ == 0 { 1 } else { 0 };
        let t = left[LEFT_CTX[blk]] + above[ABOVE_CTX[blk]];
        let nonzero = decode_block(
            b,
            &probs[typ],
            t as usize,
            first,
            dc,
            ac,
            &mut coeffs[blk * 16..blk * 16 + 16],
        );
        any |= nonzero;
        let flag = nonzero as u8;
        left[LEFT_CTX[blk]] = flag;
        above[ABOVE_CTX[blk]] = flag;
    }

    // Y2 → DC of the 16 Y blocks via inverse Walsh-Hadamard
    if has_y2 {
        let mut y2 = [0i32; 16];
        iwht(&coeffs[24 * 16..24 * 16 + 16], &mut y2);
        for i in 0..16 {
            coeffs[i * 16] = y2[i];
        }
    }
    any
}

/// Decode one 4×4 coefficient block. Returns whether any non-zero coeff was
/// written (the entropy context flag).
#[allow(clippy::too_many_arguments)]
fn decode_block(
    b: &mut Bool,
    type_probs: &[[[u8; 11]; 3]; 8],
    mut ctx: usize,
    first: usize,
    dc: i32,
    ac: i32,
    out: &mut [i32],
) -> bool {
    let mut c = first;
    let mut skip_eob = false;
    loop {
        let band = COEFF_BANDS[c];
        let p = &type_probs[band][ctx];
        if !skip_eob && b.get(p[0]) == 0 {
            break; // EOB
        }
        if b.get(p[1]) == 0 {
            // zero coefficient
            ctx = 0;
            skip_eob = true;
            c += 1;
            if c == 16 {
                break;
            }
            continue;
        }
        // non-zero value via the token tree
        let val: i32 = if b.get(p[2]) == 0 {
            ctx = 1;
            1
        } else {
            ctx = 2;
            if b.get(p[3]) == 0 {
                // 2, 3 or 4
                if b.get(p[4]) == 0 {
                    2
                } else if b.get(p[5]) == 0 {
                    3
                } else {
                    4
                }
            } else if b.get(p[6]) == 0 {
                // cat1 / cat2
                if b.get(p[7]) == 0 {
                    read_cat(b, 0)
                } else {
                    read_cat(b, 1)
                }
            } else if b.get(p[8]) == 0 {
                // cat3 / cat4
                if b.get(p[9]) == 0 {
                    read_cat(b, 2)
                } else {
                    read_cat(b, 3)
                }
            } else if b.get(p[10]) == 0 {
                read_cat(b, 4)
            } else {
                read_cat(b, 5)
            }
        };
        let signed = if b.flag() { -val } else { val };
        let dqf = if c == 0 { dc } else { ac };
        out[ZIGZAG[c]] = signed * dqf;
        skip_eob = false;
        c += 1;
        if c == 16 {
            break;
        }
    }
    // non-zero iff we advanced past the first coefficient with a written value
    c > first
}

#[inline]
fn read_cat(b: &mut Bool, cat: usize) -> i32 {
    let probs = CAT_PROBS[cat];
    let mut val = CAT_BASE[cat];
    for bc in (0..probs.len()).rev() {
        val += (b.get(probs[bc]) as i32) << bc;
    }
    val
}

// ── transforms ───────────────────────────────────────────────────────────────

const COS: i32 = 20091; // cospi8sqrt2minus1
const SIN: i32 = 35468; // sinpi8sqrt2

fn iwht(input: &[i32], output: &mut [i32; 16]) {
    let mut tmp = [0i32; 16];
    for i in 0..4 {
        let a1 = input[i] + input[12 + i];
        let b1 = input[4 + i] + input[8 + i];
        let c1 = input[4 + i] - input[8 + i];
        let d1 = input[i] - input[12 + i];
        tmp[i] = a1 + b1;
        tmp[4 + i] = c1 + d1;
        tmp[8 + i] = a1 - b1;
        tmp[12 + i] = d1 - c1;
    }
    for i in 0..4 {
        let ip = i * 4;
        let a1 = tmp[ip] + tmp[ip + 3];
        let b1 = tmp[ip + 1] + tmp[ip + 2];
        let c1 = tmp[ip + 1] - tmp[ip + 2];
        let d1 = tmp[ip] - tmp[ip + 3];
        output[ip] = (a1 + b1 + 3) >> 3;
        output[ip + 1] = (c1 + d1 + 3) >> 3;
        output[ip + 2] = (a1 - b1 + 3) >> 3;
        output[ip + 3] = (d1 - c1 + 3) >> 3;
    }
}

/// Inverse 4×4 DCT of `coeffs`, added to the prediction already in
/// `buf[pos..]` (in place), clamped.
fn idct_add(buf: &mut [u8], pos: usize, stride: usize, coeffs: &[i32]) {
    let mut tmp = [0i32; 16];
    // column pass
    for i in 0..4 {
        let a1 = coeffs[i] + coeffs[8 + i];
        let b1 = coeffs[i] - coeffs[8 + i];
        let t1 = (coeffs[4 + i] * SIN) >> 16;
        let t2 = coeffs[12 + i] + ((coeffs[12 + i] * COS) >> 16);
        let c1 = t1 - t2;
        let t1b = coeffs[4 + i] + ((coeffs[4 + i] * COS) >> 16);
        let t2b = (coeffs[12 + i] * SIN) >> 16;
        let d1 = t1b + t2b;
        tmp[i] = a1 + d1;
        tmp[12 + i] = a1 - d1;
        tmp[4 + i] = b1 + c1;
        tmp[8 + i] = b1 - c1;
    }
    // row pass + add
    for i in 0..4 {
        let ip = i * 4;
        let a1 = tmp[ip] + tmp[ip + 2];
        let b1 = tmp[ip] - tmp[ip + 2];
        let t1 = (tmp[ip + 1] * SIN) >> 16;
        let t2 = tmp[ip + 3] + ((tmp[ip + 3] * COS) >> 16);
        let c1 = t1 - t2;
        let t1b = tmp[ip + 1] + ((tmp[ip + 1] * COS) >> 16);
        let t2b = (tmp[ip + 3] * SIN) >> 16;
        let d1 = t1b + t2b;
        let r = pos + i * stride;
        buf[r] = clamp255(buf[r] as i32 + ((a1 + d1 + 4) >> 3));
        buf[r + 3] = clamp255(buf[r + 3] as i32 + ((a1 - d1 + 4) >> 3));
        buf[r + 1] = clamp255(buf[r + 1] as i32 + ((b1 + c1 + 4) >> 3));
        buf[r + 2] = clamp255(buf[r + 2] as i32 + ((b1 - c1 + 4) >> 3));
    }
}

// ── intra prediction + reconstruction ───────────────────────────────────────
// Border fixup, whole-MB (16/8) and 4×4 predictors. All operate on a plane
// buffer at a base index `p` with row stride `s`; neighbour pixels live in the
// border guaranteed by BORDER.

include!("predict.rs");

fn reconstruct_mb(
    pl: &mut Planes,
    row: usize,
    col: usize,
    mb: &Mb,
    coeffs: &mut [i32],
    mb_cols: usize,
) {
    let ys = pl.ys;
    let cs = pl.cs;
    let yp = pl.y0 + row * 16 * ys + col * 16;
    let up = pl.c0 + row * 8 * cs + col * 8;
    let vp = pl.c0 + row * 8 * cs + col * 8;

    // border fixups
    if col == 0 {
        fixup_left(&mut pl.y, yp, ys, 16, row, mb.y_mode);
        fixup_left(&mut pl.u, up, cs, 8, row, mb.uv_mode);
        fixup_left(&mut pl.v, vp, cs, 8, row, mb.uv_mode);
        if row == 0 {
            pl.y[yp - ys - 1] = 127;
        }
    }
    if row == 0 {
        fixup_above(&mut pl.y, yp, ys, 16, col, mb.y_mode);
        fixup_above(&mut pl.u, up, cs, 8, col, mb.uv_mode);
        fixup_above(&mut pl.v, vp, cs, 8, col, mb.uv_mode);
    }

    // luma
    if mb.y_mode == B_PRED {
        b_pred(&mut pl.y, yp, ys, &mb.b_modes, coeffs, col == mb_cols - 1);
    } else {
        predict_block(&mut pl.y, yp, ys, 16, mb.y_mode);
        for i in 0..16 {
            let bx = (i & 3) * 4;
            let by = (i >> 2) * 4;
            idct_add(
                &mut pl.y,
                yp + by * ys + bx,
                ys,
                &coeffs[i * 16..i * 16 + 16],
            );
        }
    }

    // chroma
    predict_block(&mut pl.u, up, cs, 8, mb.uv_mode);
    predict_block(&mut pl.v, vp, cs, 8, mb.uv_mode);
    for (j, blk) in (16..20).enumerate() {
        let bx = (j & 1) * 4;
        let by = (j >> 1) * 4;
        idct_add(
            &mut pl.u,
            up + by * cs + bx,
            cs,
            &coeffs[blk * 16..blk * 16 + 16],
        );
    }
    for (j, blk) in (20..24).enumerate() {
        let bx = (j & 1) * 4;
        let by = (j >> 1) * 4;
        idct_add(
            &mut pl.v,
            vp + by * cs + bx,
            cs,
            &coeffs[blk * 16..blk * 16 + 16],
        );
    }

    // extend the right edge of the last column's bottom row by 4px (above-right
    // source for the next row's 4×4 intra), mirroring dixie's predict_process_row.
    if col == mb_cols - 1 {
        let base = yp + 15 * ys + 16;
        let val = pl.y[yp + 15 * ys + 15];
        for k in 0..4 {
            pl.y[base + k] = val;
        }
    }
}

// ── loop filter (RFC §15, dixie_loopfilter.c) — separable deblocking post-pass ─

#[inline]
fn sat8(x: i32) -> i32 {
    x.clamp(-128, 127)
}

/// Common filter (simple filter, normal HEV path, sub-block filter). `step` is
/// the pixel stride across the edge; `pos` indexes `q0`.
fn filter_common(buf: &mut [u8], pos: usize, step: usize, outer: bool) {
    let p1 = buf[pos - 2 * step] as i32;
    let p0 = buf[pos - step] as i32;
    let q0 = buf[pos] as i32;
    let q1 = buf[pos + step] as i32;
    let mut a = 3 * (q0 - p0);
    if outer {
        a += sat8(p1 - q1);
    }
    a = sat8(a);
    let f1 = (a + 4).min(127) >> 3;
    let f2 = (a + 3).min(127) >> 3;
    buf[pos - step] = clamp255(p0 + f2);
    buf[pos] = clamp255(q0 - f1);
    if !outer {
        let a2 = (f1 + 1) >> 1;
        buf[pos - 2 * step] = clamp255(p1 + a2);
        buf[pos + step] = clamp255(q1 - a2);
    }
}

/// Macroblock-edge filter (wider, 6-tap).
fn filter_mb_edge(buf: &mut [u8], pos: usize, step: usize) {
    let p2 = buf[pos - 3 * step] as i32;
    let p1 = buf[pos - 2 * step] as i32;
    let p0 = buf[pos - step] as i32;
    let q0 = buf[pos] as i32;
    let q1 = buf[pos + step] as i32;
    let q2 = buf[pos + 2 * step] as i32;
    let w = sat8(sat8(p1 - q1) + 3 * (q0 - p0));
    let a = (27 * w + 63) >> 7;
    buf[pos - step] = clamp255(p0 + a);
    buf[pos] = clamp255(q0 - a);
    let a = (18 * w + 63) >> 7;
    buf[pos - 2 * step] = clamp255(p1 + a);
    buf[pos + step] = clamp255(q1 - a);
    let a = (9 * w + 63) >> 7;
    buf[pos - 3 * step] = clamp255(p2 + a);
    buf[pos + 2 * step] = clamp255(q2 - a);
}

#[inline]
fn hev(buf: &[u8], pos: usize, step: usize, thr: i32) -> bool {
    let p1 = buf[pos - 2 * step] as i32;
    let p0 = buf[pos - step] as i32;
    let q0 = buf[pos] as i32;
    let q1 = buf[pos + step] as i32;
    (p1 - p0).abs() > thr || (q1 - q0).abs() > thr
}

#[inline]
fn simple_thresh(buf: &[u8], pos: usize, step: usize, limit: i32) -> bool {
    let p1 = buf[pos - 2 * step] as i32;
    let p0 = buf[pos - step] as i32;
    let q0 = buf[pos] as i32;
    let q1 = buf[pos + step] as i32;
    (p0 - q0).abs() * 2 + ((p1 - q1).abs() >> 1) <= limit
}

fn normal_thresh(buf: &[u8], pos: usize, step: usize, e: i32, i: i32) -> bool {
    let pix = |k: isize| buf[(pos as isize + k * step as isize) as usize] as i32;
    simple_thresh(buf, pos, step, 2 * e + i)
        && (pix(-4) - pix(-3)).abs() <= i
        && (pix(-3) - pix(-2)).abs() <= i
        && (pix(-2) - pix(-1)).abs() <= i
        && (pix(3) - pix(2)).abs() <= i
        && (pix(2) - pix(1)).abs() <= i
        && (pix(1) - pix(0)).abs() <= i
}

/// Apply an edge filter along `count` positions. `cross` = pixel stride across
/// the edge (p/q direction); `along` = stride between successive edge positions.
#[allow(clippy::too_many_arguments)]
fn filter_edge(
    buf: &mut [u8],
    base: usize,
    cross: usize,
    along: usize,
    count: usize,
    e: i32,
    i: i32,
    hev_t: i32,
    mb_edge: bool,
) {
    for n in 0..count {
        let q0 = base + n * along;
        if normal_thresh(buf, q0, cross, e, i) {
            if mb_edge {
                if hev(buf, q0, cross, hev_t) {
                    filter_common(buf, q0, cross, true);
                } else {
                    filter_mb_edge(buf, q0, cross);
                }
            } else {
                filter_common(buf, q0, cross, hev(buf, q0, cross, hev_t));
            }
        }
    }
}

fn filter_edge_simple(buf: &mut [u8], base: usize, cross: usize, along: usize, limit: i32) {
    for n in 0..16 {
        let q0 = base + n * along;
        if simple_thresh(buf, q0, cross, limit) {
            filter_common(buf, q0, cross, true);
        }
    }
}

fn filter_params(lf: &Lf, m: &Mb) -> (i32, i32, i32) {
    let mut level = lf.level;
    if lf.seg_enabled {
        level = if lf.seg_abs {
            lf.seg_lf[m.segment]
        } else {
            level + lf.seg_lf[m.segment]
        };
    }
    level = level.clamp(0, 63);
    if lf.delta_enabled {
        level += lf.ref_delta[0]; // keyframe: ref_frame = CURRENT_FRAME (0)
        if m.y_mode == B_PRED {
            level += lf.mode_delta[0];
        }
    }
    level = level.clamp(0, 63);
    let mut interior = level;
    if lf.sharpness > 0 {
        interior >>= if lf.sharpness > 4 { 2 } else { 1 };
        if interior > 9 - lf.sharpness {
            interior = 9 - lf.sharpness;
        }
    }
    if interior < 1 {
        interior = 1;
    }
    let mut hev_t = (level >= 15) as i32;
    if level >= 40 {
        hev_t += 1;
    }
    // keyframe-only decoder: the `level >= 20 && !keyframe` bump never applies.
    (level, interior, hev_t)
}

fn loop_filter(pl: &mut Planes, mb: &[Mb], rows: usize, cols: usize, lf: &Lf) {
    if lf.level == 0 {
        return;
    }
    let (ys, cs) = (pl.ys, pl.cs);
    for row in 0..rows {
        for col in 0..cols {
            let m = mb[row * cols + col];
            let (e, i, hev_t) = filter_params(lf, &m);
            if e == 0 {
                continue;
            }
            let yp = pl.y0 + row * 16 * ys + col * 16;
            let up = pl.c0 + row * 8 * cs + col * 8;
            let sub = m.eob || m.y_mode == B_PRED;

            if lf.use_simple {
                let mb_lim = (e + 2) * 2 + i;
                let b_lim = e * 2 + i;
                if col > 0 {
                    filter_edge_simple(&mut pl.y, yp, 1, ys, mb_lim);
                }
                if sub {
                    filter_edge_simple(&mut pl.y, yp + 4, 1, ys, b_lim);
                    filter_edge_simple(&mut pl.y, yp + 8, 1, ys, b_lim);
                    filter_edge_simple(&mut pl.y, yp + 12, 1, ys, b_lim);
                }
                if row > 0 {
                    filter_edge_simple(&mut pl.y, yp, ys, 1, mb_lim);
                }
                if sub {
                    filter_edge_simple(&mut pl.y, yp + 4 * ys, ys, 1, b_lim);
                    filter_edge_simple(&mut pl.y, yp + 8 * ys, ys, 1, b_lim);
                    filter_edge_simple(&mut pl.y, yp + 12 * ys, ys, 1, b_lim);
                }
                continue;
            }

            // normal filter (Y + U + V)
            if col > 0 {
                filter_edge(&mut pl.y, yp, 1, ys, 16, e + 2, i, hev_t, true);
                filter_edge(&mut pl.u, up, 1, cs, 8, e + 2, i, hev_t, true);
                filter_edge(&mut pl.v, up, 1, cs, 8, e + 2, i, hev_t, true);
            }
            if sub {
                filter_edge(&mut pl.y, yp + 4, 1, ys, 16, e, i, hev_t, false);
                filter_edge(&mut pl.y, yp + 8, 1, ys, 16, e, i, hev_t, false);
                filter_edge(&mut pl.y, yp + 12, 1, ys, 16, e, i, hev_t, false);
                filter_edge(&mut pl.u, up + 4, 1, cs, 8, e, i, hev_t, false);
                filter_edge(&mut pl.v, up + 4, 1, cs, 8, e, i, hev_t, false);
            }
            if row > 0 {
                filter_edge(&mut pl.y, yp, ys, 1, 16, e + 2, i, hev_t, true);
                filter_edge(&mut pl.u, up, cs, 1, 8, e + 2, i, hev_t, true);
                filter_edge(&mut pl.v, up, cs, 1, 8, e + 2, i, hev_t, true);
            }
            if sub {
                filter_edge(&mut pl.y, yp + 4 * ys, ys, 1, 16, e, i, hev_t, false);
                filter_edge(&mut pl.y, yp + 8 * ys, ys, 1, 16, e, i, hev_t, false);
                filter_edge(&mut pl.y, yp + 12 * ys, ys, 1, 16, e, i, hev_t, false);
                filter_edge(&mut pl.u, up + 4 * cs, cs, 1, 8, e, i, hev_t, false);
                filter_edge(&mut pl.v, up + 4 * cs, cs, 1, 8, e, i, hev_t, false);
            }
        }
    }
}

// ── YUV420 → RGBA (full-range BT.601, as libwebp/ffmpeg webp output) ─────────
fn yuv_to_rgba(pl: &Planes) -> Vec<u8> {
    let mut out = vec![0u8; pl.w * pl.h * 4];
    for y in 0..pl.h {
        for x in 0..pl.w {
            let yy = pl.y[pl.y0 + y * pl.ys + x] as i32;
            let uu = pl.u[pl.c0 + (y / 2) * pl.cs + x / 2] as i32 - 128;
            let vv = pl.v[pl.c0 + (y / 2) * pl.cs + x / 2] as i32 - 128;
            let r = yy + ((91881 * vv) >> 16);
            let g = yy - ((22554 * uu + 46802 * vv) >> 16);
            let b = yy + ((116130 * uu) >> 16);
            let o = (y * pl.w + x) * 4;
            out[o] = clamp255(r);
            out[o + 1] = clamp255(g);
            out[o + 2] = clamp255(b);
            out[o + 3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp8_body(webp: &[u8]) -> &[u8] {
        let mut pos = 12;
        while pos + 8 <= webp.len() {
            let tag = &webp[pos..pos + 4];
            let sz = u32::from_le_bytes(webp[pos + 4..pos + 8].try_into().unwrap()) as usize;
            if tag == b"VP8 " {
                return &webp[pos + 8..pos + 8 + sz];
            }
            pos += 8 + sz + (sz & 1);
        }
        panic!("no VP8 chunk");
    }

    #[test]
    fn vp8_keyframe_matches_ffmpeg_yuv() {
        let webp = include_bytes!("../fixtures/vp8test.webp");
        let refyuv = include_bytes!("../fixtures/vp8test_ref.yuv");
        let pl = decode_to_planes(vp8_body(webp)).expect("decode");
        assert_eq!((pl.w, pl.h), (32, 32));

        let mut got = Vec::with_capacity(refyuv.len());
        for y in 0..32 {
            for x in 0..32 {
                got.push(pl.y[pl.y0 + y * pl.ys + x]);
            }
        }
        for y in 0..16 {
            for x in 0..16 {
                got.push(pl.u[pl.c0 + y * pl.cs + x]);
            }
        }
        for y in 0..16 {
            for x in 0..16 {
                got.push(pl.v[pl.c0 + y * pl.cs + x]);
            }
        }
        assert_eq!(got.len(), refyuv.len());
        let mut maxdiff = 0i32;
        let mut ndiff = 0;
        for i in 0..refyuv.len() {
            let d = (got[i] as i32 - refyuv[i] as i32).abs();
            if d > 0 {
                ndiff += 1;
            }
            maxdiff = maxdiff.max(d);
        }
        eprintln!(
            "VP8 YUV vs ffmpeg: maxdiff={maxdiff} ndiff={ndiff}/{}",
            refyuv.len()
        );
        // Bit-exact against ffmpeg's libvpx decode (full pipeline incl. the
        // deblocking loop filter §15).
        assert_eq!(maxdiff, 0, "VP8 decode must match ffmpeg YUV bit-exact");
    }

    // ── direct intra-predictor unit tests ──────────────────────────────────────
    // These exercise every 4×4 sub-block predictor and the whole-block predictor
    // in isolation, with a hand-built border region, checking the produced pixels
    // against the RFC 6386 reference formulas. The end-to-end ffmpeg test above
    // only happens to hit a subset of modes for its particular frame.

    // Reference implementations of the averaging filters (mirror predict.rs).
    fn r_avg3(a: i32, b: i32, c: i32) -> u8 {
        ((a + 2 * b + c + 2) >> 2) as u8
    }
    fn r_avg2(a: i32, b: i32) -> u8 {
        ((a + b + 1) >> 1) as u8
    }

    // Build a plane large enough for one 4×4 block at index `p` with a full
    // border: stride S=24, block at row 4, col 4 → p = 4*S + 4.
    const TS: usize = 24;
    const TP: usize = 4 * TS + 4;

    // Seed deterministic, distinct border values so wrong indexing is caught.
    // above[-1..7] live at p - s + (-1..7); left[0..3] at p + k*s - 1;
    // corner at p - s - 1.
    fn seeded_plane() -> Vec<u8> {
        let mut buf = vec![0u8; TS * TS];
        // corner
        buf[TP - TS - 1] = 100;
        // above row indices -1..=7 (note -1 == corner, set again for clarity)
        let above = [100, 10, 20, 30, 40, 50, 60, 70, 80]; // k = -1,0,1,2,3,4,5,6,7
        for (idx, &v) in above.iter().enumerate() {
            // idx 0 → k=-1 → p-s-1 ; idx 1 → k=0 → p-s ; etc.
            buf[TP - TS - 1 + idx] = v;
        }
        // left column rows 0..3 at p + k*s - 1
        let left = [200, 190, 180, 170];
        for (k, &v) in left.iter().enumerate() {
            buf[TP + k * TS - 1] = v;
        }
        buf
    }

    fn block4(buf: &[u8]) -> [[u8; 4]; 4] {
        let mut g = [[0u8; 4]; 4];
        for (r, row) in g.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = buf[TP + r * TS + c];
            }
        }
        g
    }

    #[test]
    fn pred4_ve_matches_reference() {
        let mut buf = seeded_plane();
        pred4_ve(&mut buf, TP, TS);
        // a(k) = above[k], k=-1..4 → 100,10,20,30,40,50
        let expect_row = [
            r_avg3(100, 10, 20),
            r_avg3(10, 20, 30),
            r_avg3(20, 30, 40),
            r_avg3(30, 40, 50),
        ];
        let g = block4(&buf);
        for (r, row) in g.iter().enumerate() {
            assert_eq!(
                *row, expect_row,
                "VE row {r} must be vertical-filtered above"
            );
        }
    }

    #[test]
    fn pred4_he_matches_reference() {
        let mut buf = seeded_plane();
        pred4_he(&mut buf, TP, TS);
        // corner=100, left=200,190,180,170
        let rows = [
            r_avg3(100, 200, 190),
            r_avg3(200, 190, 180),
            r_avg3(190, 180, 170),
            r_avg3(180, 170, 170),
        ];
        let g = block4(&buf);
        for r in 0..4 {
            assert_eq!(g[r], [rows[r]; 4], "HE row {r} constant = filtered left");
        }
    }

    #[test]
    fn pred4_ld_matches_reference() {
        let mut buf = seeded_plane();
        pred4_ld(&mut buf, TP, TS);
        // a(0..7) = 10,20,30,40,50,60,70,80
        let pr = [
            r_avg3(10, 20, 30),
            r_avg3(20, 30, 40),
            r_avg3(30, 40, 50),
            r_avg3(40, 50, 60),
            r_avg3(50, 60, 70),
            r_avg3(60, 70, 80),
            r_avg3(70, 80, 80),
        ];
        let g = block4(&buf);
        for r in 0..4 {
            for c in 0..4 {
                assert_eq!(g[r][c], pr[r + c], "LD[{r}][{c}] diagonal");
            }
        }
    }

    #[test]
    fn pred4_rd_matches_reference() {
        let mut buf = seeded_plane();
        pred4_rd(&mut buf, TP, TS);
        // l(3..0)=170,180,190,200 ; a(-1..3)=100,10,20,30
        let d = [
            r_avg3(170, 180, 190),
            r_avg3(180, 190, 200),
            r_avg3(190, 200, 100),
            r_avg3(200, 100, 10),
            r_avg3(100, 10, 20),
            r_avg3(10, 20, 30),
            r_avg3(20, 30, 40),
        ];
        let g = block4(&buf);
        for r in 0..4 {
            for c in 0..4 {
                assert_eq!(g[r][c], d[3 - r + c], "RD[{r}][{c}]");
            }
        }
    }

    #[test]
    fn pred4_vr_vl_hd_hu_are_deterministic_and_in_range() {
        // These four write via write4; verify they run, mutate the block, and
        // stay within the writable region (no panic ⇒ indices valid).
        for f in [
            pred4_vr as fn(&mut [u8], usize, usize),
            pred4_vl,
            pred4_hd,
            pred4_hu,
        ] {
            let mut buf = seeded_plane();
            f(&mut buf, TP, TS);
            // at least one cell must be non-zero (border was non-zero)
            let g = block4(&buf);
            assert!(g.iter().flatten().any(|&v| v != 0));
            // pixels outside the 4×4 block must remain untouched at a far cell
            assert_eq!(buf[TP + 6 * TS + 6], 0);
        }
    }

    #[test]
    fn pred4_vr_first_row_matches_reference() {
        let mut buf = seeded_plane();
        pred4_vr(&mut buf, TP, TS);
        let g = block4(&buf);
        // row 0 = avg2(a(-1),a(0)), avg2(a(0),a(1)), avg2(a(1),a(2)), avg2(a(2),a(3))
        assert_eq!(
            g[0],
            [
                r_avg2(100, 10),
                r_avg2(10, 20),
                r_avg2(20, 30),
                r_avg2(30, 40)
            ]
        );
    }

    #[test]
    fn pred4_vl_first_row_matches_reference() {
        let mut buf = seeded_plane();
        pred4_vl(&mut buf, TP, TS);
        let g = block4(&buf);
        assert_eq!(
            g[0],
            [
                r_avg2(10, 20),
                r_avg2(20, 30),
                r_avg2(30, 40),
                r_avg2(40, 50)
            ]
        );
    }

    #[test]
    fn pred4_hu_last_row_is_l3() {
        let mut buf = seeded_plane();
        pred4_hu(&mut buf, TP, TS);
        let g = block4(&buf);
        // bottom row is all l(3) = 170
        assert_eq!(g[3], [170u8; 4]);
    }

    #[test]
    fn pred4_hd_first_cell_matches_reference() {
        let mut buf = seeded_plane();
        pred4_hd(&mut buf, TP, TS);
        let g = block4(&buf);
        // p0 = avg2(l(0), a(-1)) = avg2(200,100)
        assert_eq!(g[0][0], r_avg2(200, 100));
    }

    #[test]
    fn predict_block_v_h_tm_dc() {
        // V_PRED copies the above row across all rows.
        let mut buf = seeded_plane();
        predict_block(&mut buf, TP, TS, 4, V_PRED);
        let g = block4(&buf);
        for (r, row) in g.iter().enumerate() {
            assert_eq!(*row, [10, 20, 30, 40], "V_PRED row {r}");
        }
        // H_PRED replicates each left pixel across its row.
        let mut buf = seeded_plane();
        predict_block(&mut buf, TP, TS, 4, H_PRED);
        let g = block4(&buf);
        assert_eq!(g[0], [200; 4]);
        assert_eq!(g[1], [190; 4]);
        assert_eq!(g[2], [180; 4]);
        assert_eq!(g[3], [170; 4]);
        // TM_PRED = clamp(left + above - corner).
        let mut buf = seeded_plane();
        predict_block(&mut buf, TP, TS, 4, TM_PRED);
        let g = block4(&buf);
        // row0 col0 = clamp(200 + 10 - 100) = 110
        assert_eq!(g[0][0], 110);
        // DC_PRED 4×4: dc = sum(left)+sum(above[0..3]) then (dc+4)>>3.
        let mut buf = seeded_plane();
        predict_block(&mut buf, TP, TS, 4, DC_PRED);
        let g = block4(&buf);
        let dc = 200 + 190 + 180 + 170 + 10 + 20 + 30 + 40;
        let dcv = ((dc + 4) >> 3) as u8;
        for (r, row) in g.iter().enumerate() {
            assert_eq!(*row, [dcv; 4], "DC row {r}");
        }
    }

    #[test]
    fn predict_block_dc_shift_for_8_and_16() {
        // shift = 4 for n=8, 5 for n=16. Build a wider border.
        const S: usize = 40;
        const P: usize = 4 * S + 4;
        let mut buf = vec![5u8; S * S];
        // above row n entries + left col n entries all = 5
        predict_block(&mut buf, P, S, 8, DC_PRED);
        // dc = 8*5 (left) + 8*5 (above) = 80 ; (80 + 8) >> 4 = 5
        assert_eq!(buf[P], 5);
        let mut buf = vec![5u8; S * S];
        predict_block(&mut buf, P, S, 16, DC_PRED);
        // dc = 16*5 + 16*5 = 160 ; (160 + 16) >> 5 = 5
        assert_eq!(buf[P], 5);
    }

    #[test]
    fn fixup_left_dc_and_other() {
        const S: usize = 24;
        const P: usize = 4 * S + 4;
        // DC_PRED with row>0 copies above row into left column.
        let mut buf = vec![0u8; S * S];
        for i in 0..4 {
            buf[P - S + i] = (i as u8 + 1) * 11;
        }
        fixup_left(&mut buf, P, S, 4, 1, DC_PRED);
        for i in 0..4 {
            assert_eq!(buf[P + i * S - 1], (i as u8 + 1) * 11);
        }
        // Non-DC (or row 0): left column incl. corner filled with 129.
        let mut buf = vec![0u8; S * S];
        fixup_left(&mut buf, P, S, 4, 0, V_PRED);
        let start = P - 1 - S;
        for i in 0..=4 {
            assert_eq!(buf[start + i * S], 129);
        }
    }

    #[test]
    fn fixup_above_dc_and_other() {
        const S: usize = 24;
        const P: usize = 4 * S + 4;
        // DC_PRED with col>0 copies left column into above row.
        let mut buf = vec![0u8; S * S];
        for i in 0..4 {
            buf[P - 1 + i * S] = (i as u8 + 1) * 7;
        }
        fixup_above(&mut buf, P, S, 4, 1, DC_PRED);
        for i in 0..4 {
            assert_eq!(buf[P - S + i], (i as u8 + 1) * 7);
        }
        // above-right 4px always 127
        for i in 0..4 {
            assert_eq!(buf[P - S + 4 + i], 127);
        }
        // Non-DC (or col 0): above row incl. corner = 127.
        let mut buf = vec![0u8; S * S];
        fixup_above(&mut buf, P, S, 4, 0, V_PRED);
        for i in 0..=4 {
            assert_eq!(buf[P - S - 1 + i], 127);
        }
    }
}
