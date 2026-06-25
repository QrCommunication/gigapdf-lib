//! End-to-end JPEG 2000 decode tests with in-test encoders (no third-party
//! data). A minimal encoder builds a valid codestream from a known raster:
//! a forward DWT (5/3 reversible / 9/7 irreversible), a tier-1 EBCOT encoder
//! that mirrors the decoder's three passes bit-for-bit through an MQ *encoder*
//! (the inverse of the shared MQ decoder), and a tier-2 packet/codestream
//! writer. The decoder must then recover the original pixels.
//!
//! `ArithContext`'s `index`/`mps` fields are crate-private; this module is in
//! the same crate, so the encoder reads/writes them directly.

use super::decode_to_image;
use crate::filters::jbig2_mq::{qe_entry_for_test, ArithContext};

// ===========================================================================
// MQ arithmetic encoder (ISO/IEC 15444-1 Annex C / ITU-T T.88 Annex E.3.1) —
// the exact inverse of the shared `MqDecoder`.
// ===========================================================================

struct MqEncoder {
    a: u32,
    c: u32,
    ct: i32,
    b: u8,
    bp_valid: bool,
    out: Vec<u8>,
}

impl MqEncoder {
    fn new() -> Self {
        MqEncoder {
            a: 0x8000,
            c: 0,
            ct: 12,
            b: 0,
            bp_valid: false,
            out: Vec::new(),
        }
    }

    fn encode(&mut self, cx: &mut ArithContext, d: u8) {
        let (qe, nmps, nlps, switch) = qe_entry_for_test(cx.index);
        self.a = self.a.wrapping_sub(qe);
        if d == cx.mps {
            if self.a & 0x8000 == 0 {
                if self.a < qe {
                    self.a = qe;
                } else {
                    self.c += qe;
                }
                cx.index = nmps;
                self.renorme();
            } else {
                self.c += qe;
            }
        } else {
            if self.a < qe {
                self.c += qe;
            } else {
                self.a = qe;
            }
            if switch == 1 {
                cx.mps = 1 - cx.mps;
            }
            cx.index = nlps;
            self.renorme();
        }
    }

    fn renorme(&mut self) {
        loop {
            if self.ct == 0 {
                self.byteout();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }

    fn byteout(&mut self) {
        if self.b == 0xFF {
            self.emit_b();
            self.b = ((self.c >> 20) & 0xFF) as u8;
            self.c &= 0xF_FFFF;
            self.ct = 7;
        } else if self.c & 0x0800_0000 != 0 {
            let nb = self.b.wrapping_add(1);
            self.b = nb;
            if nb == 0xFF {
                self.emit_b();
                self.b = ((self.c >> 20) & 0xFF) as u8;
                self.c &= 0xF_FFFF;
                self.ct = 7;
            } else {
                self.emit_b();
                self.b = ((self.c >> 19) & 0xFF) as u8;
                self.c &= 0x7_FFFF;
                self.ct = 8;
            }
        } else {
            self.emit_b();
            self.b = ((self.c >> 19) & 0xFF) as u8;
            self.c &= 0x7_FFFF;
            self.ct = 8;
        }
    }

    fn emit_b(&mut self) {
        if self.bp_valid {
            self.out.push(self.b);
        }
        self.bp_valid = true;
    }

    fn finish(mut self) -> Vec<u8> {
        let tempc = self.c + self.a;
        self.c |= 0xFFFF;
        if self.c >= tempc {
            self.c -= 0x8000;
        }
        self.c <<= self.ct;
        self.byteout();
        self.c <<= self.ct;
        self.byteout();
        self.emit_b();
        self.out
    }
}

// ===========================================================================
// Tier-1 EBCOT encoder — mirrors the decoder's pass logic and context model
// exactly so the MQ bit sequence aligns. Encodes signed coefficients across
// `numbps` bit-planes.
// ===========================================================================

const SIG: u8 = 1;
const VISIT: u8 = 2;
const REFINED: u8 = 4;
const CTX_RL: usize = 17;
const CTX_UNI: usize = 18;
const CTX_SIGN_BASE: usize = 9;
const CTX_MR_BASE: usize = 14;

struct T1Enc {
    w: usize,
    h: usize,
    orient: u8,
    flags: Vec<u8>,
    /// Final coefficient magnitudes and signs (the data to encode).
    mag: Vec<u32>,
    sign: Vec<bool>,
    ctx: [ArithContext; 19],
    enc: MqEncoder,
}

impl T1Enc {
    fn new(w: usize, h: usize, orient: u8, mag: Vec<u32>, sign: Vec<bool>) -> Self {
        let mut ctx = [ArithContext::default(); 19];
        ctx[CTX_UNI] = ArithContext::with(46, 0);
        ctx[CTX_RL] = ArithContext::with(3, 0);
        ctx[0] = ArithContext::with(4, 0);
        T1Enc {
            w,
            h,
            orient,
            flags: vec![0u8; w * h],
            mag,
            sign,
            ctx,
            enc: MqEncoder::new(),
        }
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.w + x
    }

    fn is_sig(&self, x: i64, y: i64) -> bool {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return false;
        }
        self.flags[(y as usize) * self.w + x as usize] & SIG != 0
    }

    fn sign_of(&self, x: i64, y: i64) -> i32 {
        if !self.is_sig(x, y) {
            return 0;
        }
        let i = (y as usize) * self.w + x as usize;
        if self.sign[i] {
            -1
        } else {
            1
        }
    }

    fn bit_at(&self, i: usize, p: u32) -> u8 {
        ((self.mag[i] >> p) & 1) as u8
    }

    fn encode_planes(mut self, p0: u32, numbps: u32) -> Vec<u8> {
        // Pass 0: cleanup of the most significant plane.
        self.cleanup(p0);
        // Then for each lower plane: SP, MR, CU.
        let mut p = p0 as i64 - 1;
        let mut planes_done = 1;
        while planes_done < numbps && p >= 0 {
            self.sig_prop(p as u32);
            self.mag_ref(p as u32);
            self.cleanup(p as u32);
            planes_done += 1;
            p -= 1;
        }
        self.enc.finish()
    }

    fn sig_prop(&mut self, p: u32) {
        for y0 in (0..self.h).step_by(4) {
            for x in 0..self.w {
                for y in y0..(y0 + 4).min(self.h) {
                    let i = self.idx(x, y);
                    self.flags[i] &= !VISIT;
                    if self.flags[i] & SIG != 0 {
                        continue;
                    }
                    if !self.has_sig_neighbour(x, y) {
                        continue;
                    }
                    let cx = self.zc_context(x, y);
                    let bit = self.bit_at(i, p);
                    self.enc.encode_ctx(&mut self.ctx, cx, bit);
                    if bit == 1 {
                        self.encode_sign(x, y);
                        let i = self.idx(x, y);
                        self.flags[i] |= SIG;
                    }
                    let i = self.idx(x, y);
                    self.flags[i] |= VISIT;
                }
            }
        }
    }

    fn mag_ref(&mut self, p: u32) {
        for y0 in (0..self.h).step_by(4) {
            for x in 0..self.w {
                for y in y0..(y0 + 4).min(self.h) {
                    let i = self.idx(x, y);
                    if self.flags[i] & SIG == 0 {
                        continue;
                    }
                    if self.flags[i] & VISIT != 0 {
                        continue;
                    }
                    let cx = self.mr_context(x, y);
                    let bit = self.bit_at(i, p);
                    self.enc.encode_ctx(&mut self.ctx, cx, bit);
                    let i = self.idx(x, y);
                    self.flags[i] |= REFINED;
                }
            }
        }
    }

    fn cleanup(&mut self, p: u32) {
        for y0 in (0..self.h).step_by(4) {
            for x in 0..self.w {
                let strip_h = (y0 + 4).min(self.h) - y0;
                let mut y = y0;
                if strip_h == 4 && self.strip_all_insig_no_neighbour(x, y0) {
                    // Find the first significant row in the strip at this plane.
                    let mut first = None;
                    for (k, yy) in (y0..y0 + 4).enumerate() {
                        let i = self.idx(x, yy);
                        if self.bit_at(i, p) == 1 {
                            first = Some(k);
                            break;
                        }
                    }
                    if let Some(run) = first {
                        self.enc.encode_ctx(&mut self.ctx, CTX_RL, 1);
                        let hi = (run >> 1) as u8 & 1;
                        let lo = run as u8 & 1;
                        self.enc.encode_ctx(&mut self.ctx, CTX_UNI, hi);
                        self.enc.encode_ctx(&mut self.ctx, CTX_UNI, lo);
                        y = y0 + run;
                        self.encode_sign(x, y);
                        let i = self.idx(x, y);
                        self.flags[i] |= SIG;
                        self.flags[i] &= !VISIT;
                        y += 1;
                    } else {
                        self.enc.encode_ctx(&mut self.ctx, CTX_RL, 0);
                        for yy in y0..y0 + 4 {
                            let i = self.idx(x, yy);
                            self.flags[i] &= !VISIT;
                        }
                        continue;
                    }
                }
                while y < (y0 + 4).min(self.h) {
                    let i = self.idx(x, y);
                    if self.flags[i] & (SIG | VISIT) != 0 {
                        self.flags[i] &= !VISIT;
                        y += 1;
                        continue;
                    }
                    let cx = self.zc_context(x, y);
                    let bit = self.bit_at(i, p);
                    self.enc.encode_ctx(&mut self.ctx, cx, bit);
                    if bit == 1 {
                        self.encode_sign(x, y);
                        let i = self.idx(x, y);
                        self.flags[i] |= SIG;
                    }
                    let i = self.idx(x, y);
                    self.flags[i] &= !VISIT;
                    y += 1;
                }
            }
        }
    }

    fn strip_all_insig_no_neighbour(&self, x: usize, y0: usize) -> bool {
        for y in y0..y0 + 4 {
            let i = self.idx(x, y);
            if self.flags[i] & SIG != 0 {
                return false;
            }
            if self.has_sig_neighbour(x, y) {
                return false;
            }
        }
        true
    }

    fn encode_sign(&mut self, x: usize, y: usize) {
        let (ctx, xor) = self.sign_context(x, y);
        let i = self.idx(x, y);
        let s = self.sign[i] as i32; // 1 = negative
        let bit = (s ^ xor) as u8;
        self.enc.encode_ctx(&mut self.ctx, ctx, bit);
    }

    fn has_sig_neighbour(&self, x: usize, y: usize) -> bool {
        let (xi, yi) = (x as i64, y as i64);
        self.is_sig(xi - 1, yi)
            || self.is_sig(xi + 1, yi)
            || self.is_sig(xi, yi - 1)
            || self.is_sig(xi, yi + 1)
            || self.is_sig(xi - 1, yi - 1)
            || self.is_sig(xi + 1, yi - 1)
            || self.is_sig(xi - 1, yi + 1)
            || self.is_sig(xi + 1, yi + 1)
    }

    fn zc_context(&self, x: usize, y: usize) -> usize {
        let (xi, yi) = (x as i64, y as i64);
        let hh = self.is_sig(xi - 1, yi) as u32 + self.is_sig(xi + 1, yi) as u32;
        let vv = self.is_sig(xi, yi - 1) as u32 + self.is_sig(xi, yi + 1) as u32;
        let dd = self.is_sig(xi - 1, yi - 1) as u32
            + self.is_sig(xi + 1, yi - 1) as u32
            + self.is_sig(xi - 1, yi + 1) as u32
            + self.is_sig(xi + 1, yi + 1) as u32;
        super::t1::zc_label_for_test(self.orient, hh, vv, dd)
    }

    fn sign_context(&self, x: usize, y: usize) -> (usize, i32) {
        let (xi, yi) = (x as i64, y as i64);
        let h = (self.sign_of(xi - 1, yi) + self.sign_of(xi + 1, yi)).clamp(-1, 1);
        let v = (self.sign_of(xi, yi - 1) + self.sign_of(xi, yi + 1)).clamp(-1, 1);
        let (off, xor) = match (h, v) {
            (1, 1) => (4, 0),
            (1, 0) => (3, 0),
            (1, -1) => (2, 0),
            (0, 1) => (1, 0),
            (0, 0) => (0, 0),
            (0, -1) => (1, 1),
            (-1, 1) => (2, 1),
            (-1, 0) => (3, 1),
            (-1, -1) => (4, 1),
            _ => (0, 0),
        };
        (CTX_SIGN_BASE + off, xor)
    }

    fn mr_context(&self, x: usize, y: usize) -> usize {
        let i = self.idx(x, y);
        if self.flags[i] & REFINED == 0 {
            CTX_MR_BASE + self.has_sig_neighbour(x, y) as usize
        } else {
            CTX_MR_BASE + 2
        }
    }
}

impl MqEncoder {
    fn encode_ctx(&mut self, ctx: &mut [ArithContext; 19], i: usize, bit: u8) {
        let mut c = ctx[i];
        self.encode(&mut c, bit);
        ctx[i] = c;
    }
}

// ===========================================================================
// Forward DWT (the inverse of `super::dwt`'s synthesis), used to build the
// subband coefficients from a spatial tile-component.
// ===========================================================================

const ALPHA: f64 = -1.586_134_342_059_924;
const BETA: f64 = -0.052_980_118_572_961;
const GAMMA: f64 = 0.882_911_075_530_934;
const DELTA: f64 = 0.443_506_852_043_971;
const KK: f64 = 1.230_174_104_914_001;

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

/// Forward 5/3: inverse of `idwt_53`. Interleaved layout, low at even (cas 0).
fn fwd_53(x: &mut [f64]) {
    let n = x.len();
    if n < 2 {
        return;
    }
    // Undo predict, then undo update (reverse order, opposite sign).
    for i in (0..n).filter(|i| i % 2 == 1) {
        let sl = x[mirror(i as isize - 1, n)];
        let sr = x[mirror(i as isize + 1, n)];
        x[i] -= ((sl + sr) / 2.0).floor();
    }
    for i in (0..n).filter(|i| i % 2 == 0) {
        let dl = x[mirror(i as isize - 1, n)];
        let dr = x[mirror(i as isize + 1, n)];
        x[i] += ((dl + dr + 2.0) / 4.0).floor();
    }
}

fn lift(x: &mut [f64], n: usize, target_low: bool, coeff: f64) {
    for i in 0..n {
        let is_low = i % 2 == 0;
        if is_low != target_low {
            continue;
        }
        let nl = x[mirror(i as isize - 1, n)];
        let nr = x[mirror(i as isize + 1, n)];
        x[i] += coeff * (nl + nr);
    }
}

/// Forward 9/7: inverse of `idwt_97`.
fn fwd_97(x: &mut [f64]) {
    let n = x.len();
    if n < 2 {
        if n == 1 {
            x[0] *= KK;
        }
        return;
    }
    lift(x, n, false, ALPHA);
    lift(x, n, true, BETA);
    lift(x, n, false, GAMMA);
    lift(x, n, true, DELTA);
    for (i, xi) in x.iter_mut().enumerate() {
        if i % 2 == 0 {
            *xi *= KK;
        } else {
            *xi /= KK;
        }
    }
}

/// Deinterleave an interleaved DWT line (low at even indices, high at odd) into
/// the blocked layout `[lows…, highs…]` (Mallat order), matching how the decoder
/// reads each subband as a contiguous half-resolution plane.
fn deinterleave(line: &[f64]) -> Vec<f64> {
    let n = line.len();
    let sn = n.div_ceil(2);
    let mut out = vec![0.0; n];
    for (k, slot) in out.iter_mut().enumerate().take(sn) {
        *slot = line[2 * k];
    }
    for k in 0..(n - sn) {
        out[sn + k] = line[2 * k + 1];
    }
    out
}

/// One level of forward 2D DWT on a `w×h` plane (rows then columns), returning
/// the Mallat-blocked transform: LL top-left, HL top-right, LH bottom-left,
/// HH bottom-right.
fn fwd_dwt_level(plane: &[f64], w: usize, h: usize, reversible: bool) -> Vec<f64> {
    let mut buf = plane.to_vec();
    // Rows: lift then deinterleave horizontally.
    let mut row = vec![0.0; w];
    for y in 0..h {
        row.copy_from_slice(&buf[y * w..y * w + w]);
        if reversible {
            fwd_53(&mut row);
        } else {
            fwd_97(&mut row);
        }
        let de = deinterleave(&row);
        buf[y * w..y * w + w].copy_from_slice(&de);
    }
    // Columns: lift then deinterleave vertically.
    let mut col = vec![0.0; h];
    for x in 0..w {
        for y in 0..h {
            col[y] = buf[y * w + x];
        }
        if reversible {
            fwd_53(&mut col);
        } else {
            fwd_97(&mut col);
        }
        let de = deinterleave(&col);
        for y in 0..h {
            buf[y * w + x] = de[y];
        }
    }
    buf
}

// ===========================================================================
// Codestream assembly.
// ===========================================================================

fn be16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn be32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Parameters for a single-precinct, single-layer, one-code-block-per-subband
/// codestream. Supports a `tiles_x × tiles_y` tile grid (the image dimensions
/// must divide evenly so tile origins stay even — matching the encoder's
/// origin-0 DWT parity assumption).
struct Encoded {
    w: u32,
    h: u32,
    bit_depth: u32,
    ncomp: usize,
    levels: u32,
    reversible: bool,
    mct: bool,
    tiles_x: u32,
    tiles_y: u32,
    /// Per-component spatial samples (already level-shifted out — raw signed).
    comp_samples: Vec<Vec<i32>>,
}

impl Encoded {
    /// A single-tile encoding (the common case).
    #[allow(clippy::too_many_arguments)]
    fn single_tile(
        w: u32,
        h: u32,
        bit_depth: u32,
        ncomp: usize,
        levels: u32,
        reversible: bool,
        mct: bool,
        comp_samples: Vec<Vec<i32>>,
    ) -> Self {
        Encoded {
            w,
            h,
            bit_depth,
            ncomp,
            levels,
            reversible,
            mct,
            tiles_x: 1,
            tiles_y: 1,
            comp_samples,
        }
    }

    /// Build a complete codestream and return its bytes.
    fn codestream(&self) -> Vec<u8> {
        let guard = 2u32;
        let xtsiz = self.w / self.tiles_x;
        let ytsiz = self.h / self.tiles_y;
        let mut out = Vec::new();
        // SOC.
        be16(0xFF4F, &mut out);
        // SIZ.
        be16(0xFF51, &mut out);
        be16((38 + 3 * self.ncomp) as u16, &mut out);
        be16(0, &mut out); // Rsiz
        be32(self.w, &mut out); // Xsiz
        be32(self.h, &mut out); // Ysiz
        be32(0, &mut out); // XOsiz
        be32(0, &mut out); // YOsiz
        be32(xtsiz, &mut out); // XTsiz
        be32(ytsiz, &mut out); // YTsiz
        be32(0, &mut out); // XTOsiz
        be32(0, &mut out); // YTOsiz
        be16(self.ncomp as u16, &mut out);
        for _ in 0..self.ncomp {
            out.push((self.bit_depth - 1) as u8); // Ssiz (unsigned)
            out.push(1); // XRsiz
            out.push(1); // YRsiz
        }
        // COD. SPcod code-block size fields are the exponent minus 2: a value of
        // 4 gives a 2^(4+2)=64-sample code-block side — large enough that each
        // subband fits in a single code-block for these small test images.
        let cb_exp = 4u8;
        be16(0xFF52, &mut out);
        be16(12, &mut out); // Lcod (no precinct sizes)
        out.push(0x00); // Scod: no SOP/EPH, no defined precincts
        out.push(0x00); // progression LRCP
        be16(1, &mut out); // num layers
        out.push(if self.mct { 1 } else { 0 }); // MCT
        out.push(self.levels as u8); // decomposition levels
        out.push(cb_exp); // code-block width exponent − 2
        out.push(cb_exp); // code-block height exponent − 2
        out.push(0x00); // cb style
        out.push(if self.reversible { 1 } else { 0 }); // transform
                                                       // QCD: no-quantisation (reversible) uses 8-bit exponents per subband;
                                                       // irreversible uses expounded 16-bit words. Provide an exponent per
                                                       // subband (LL + 3·levels).
        let nbands = 1 + 3 * self.levels as usize;
        be16(0xFF5C, &mut out);
        if self.reversible {
            be16((3 + nbands) as u16, &mut out); // Lqcd = 2 + 1 + nbands
            out.push((guard as u8) << 5); // SQcd: style 0, guard bits
            for b in 0..nbands {
                let gain = band_gain(b, self.levels);
                let exp = self.bit_depth + gain; // ε_b for reversible
                out.push((exp as u8) << 3);
            }
        } else {
            be16((3 + 2 * nbands) as u16, &mut out); // Lqcd = 2 + 1 + 2*nbands
            out.push((guard as u8) << 5 | 0x02); // style 2 (expounded)
            for b in 0..nbands {
                let gain = band_gain(b, self.levels);
                let exp = self.bit_depth + gain + 1;
                let word: u16 = (exp as u16) << 11; // mantissa 0
                be16(word, &mut out);
            }
        }
        // One tile-part per tile, in raster order (tile index = ty*tiles_x+tx).
        for ty in 0..self.tiles_y {
            for tx in 0..self.tiles_x {
                let tile_index = ty * self.tiles_x + tx;
                let tw = xtsiz as usize;
                let th = ytsiz as usize;
                let tile_samples = self.extract_tile(tx, ty, tw, th);
                let body = build_packets(
                    self.ncomp,
                    self.levels,
                    self.bit_depth,
                    self.reversible,
                    tw,
                    th,
                    &tile_samples,
                );
                // SOT.
                be16(0xFF90, &mut out);
                be16(10, &mut out); // Lsot
                be16(tile_index as u16, &mut out); // Isot
                let psot_pos = out.len();
                be32(0, &mut out); // Psot placeholder
                out.push(0); // TPsot
                out.push(1); // TNsot
                be16(0xFF93, &mut out); // SOD
                                        // The SOT marker is 6 bytes before `psot_pos` (marker + Lsot +
                                        // Isot); `Psot` spans from there to the end of the tile-part.
                let sot_marker_pos = psot_pos - 6;
                out.extend_from_slice(&body);
                let psot = (out.len() - sot_marker_pos) as u32;
                out[psot_pos..psot_pos + 4].copy_from_slice(&psot.to_be_bytes());
            }
        }
        // EOC.
        be16(0xFFD9, &mut out);
        out
    }

    /// Extract a tile's per-component samples (origin-relative).
    fn extract_tile(&self, tx: u32, ty: u32, tw: usize, th: usize) -> Vec<Vec<i32>> {
        let w = self.w as usize;
        let ox = tx as usize * tw;
        let oy = ty as usize * th;
        let mut out = Vec::with_capacity(self.ncomp);
        for c in 0..self.ncomp {
            let mut tile = vec![0i32; tw * th];
            for yy in 0..th {
                for xx in 0..tw {
                    tile[yy * tw + xx] = self.comp_samples[c][(oy + yy) * w + (ox + xx)];
                }
            }
            out.push(tile);
        }
        out
    }
}

/// Build a tile body: one packet per (resolution, component) in LRCP order
/// (outer resolution, inner component), each containing that (component,
/// resolution)'s subbands. Mirrors the decoder's iteration.
#[allow(clippy::too_many_arguments)]
fn build_packets(
    ncomp: usize,
    levels: u32,
    bit_depth: u32,
    reversible: bool,
    tw: usize,
    th: usize,
    samples: &[Vec<i32>],
) -> Vec<u8> {
    let bands = encode_subbands(ncomp, levels, bit_depth, reversible, tw, th, samples);
    let numres = levels as usize + 1;
    let mut out = Vec::new();
    for res in 0..numres {
        for comp in 0..ncomp {
            let group: Vec<&EncodedBand> = bands
                .iter()
                .filter(|b| b.comp == comp && b.res == res)
                .collect();
            out.extend_from_slice(&encode_one_packet(&group));
        }
    }
    out
}

/// For each subband of a tile, run the forward DWT and tier-1 encode its single
/// code-block, returning the coded bytes and tier-2 parameters.
#[allow(clippy::too_many_arguments)]
fn encode_subbands(
    ncomp: usize,
    levels: u32,
    bit_depth: u32,
    reversible: bool,
    w: usize,
    h: usize,
    samples: &[Vec<i32>],
) -> Vec<EncodedBand> {
    let mut bands = Vec::new();
    let guard = 2u32;
    for (c, comp_samples) in samples.iter().enumerate().take(ncomp) {
        let mut plane: Vec<f64> = comp_samples.iter().map(|&v| v as f64).collect();
        // Apply the forward DWT level by level on the current LL region: at
        // each level the LL of the previous level is transformed in place.
        let mut cur_w = w;
        let mut cur_h = h;
        let mut full = plane.clone();
        for _ in 0..levels {
            let mut region = vec![0.0; cur_w * cur_h];
            for y in 0..cur_h {
                for x in 0..cur_w {
                    region[y * cur_w + x] = full[y * w + x];
                }
            }
            let t = fwd_dwt_level(&region, cur_w, cur_h, reversible);
            for y in 0..cur_h {
                for x in 0..cur_w {
                    full[y * w + x] = t[y * cur_w + x];
                }
            }
            cur_w = cur_w.div_ceil(2);
            cur_h = cur_h.div_ceil(2);
        }
        plane = full;

        // Extract each subband from the Mallat pyramid and tier-1-encode it.
        for spec in &subband_layout(w, h, levels) {
            let bw = spec.x1 - spec.x0;
            let bh = spec.y1 - spec.y0;
            let gain = spec.gain;
            let exp = if reversible {
                bit_depth + gain
            } else {
                bit_depth + gain + 1
            };
            // Irreversible quantisation step Δ_b (mantissa 0), matching the
            // decoder's dequant: Δ = 2^(Rb − ε_b), Rb = bit_depth + gain.
            let delta = if reversible {
                1.0
            } else {
                2f64.powi((bit_depth + gain) as i32 - exp as i32)
            };
            let mut mag = vec![0u32; bw * bh];
            let mut sign = vec![false; bw * bh];
            let mut maxmag = 0u32;
            for yy in 0..bh {
                for xx in 0..bw {
                    let v = plane[(spec.y0 + yy) * w + (spec.x0 + xx)];
                    let q = (v / delta).round() as i64;
                    let m = q.unsigned_abs() as u32;
                    mag[yy * bw + xx] = m;
                    sign[yy * bw + xx] = q < 0;
                    maxmag = maxmag.max(m);
                }
            }
            let mb = guard + exp - 1;
            let msb = if maxmag == 0 {
                0
            } else {
                31 - maxmag.leading_zeros()
            };
            let zbp = mb.saturating_sub(1).saturating_sub(msb);
            let p0 = mb - 1 - zbp;
            let numbps = p0 + 1; // decode planes p0..0
            let passes = if maxmag == 0 { 1 } else { 3 * numbps - 2 };
            let nbps = if maxmag == 0 { 1 } else { numbps };
            let enc = T1Enc::new(bw, bh, spec.orient_kind, mag, sign);
            let data = enc.encode_planes(p0, nbps);
            bands.push(EncodedBand {
                comp: c,
                res: spec.res,
                zbp,
                passes,
                data,
            });
        }
    }
    bands
}

struct EncodedBand {
    comp: usize,
    res: usize,
    zbp: u32,
    passes: u32,
    data: Vec<u8>,
}

/// Encode one packet from its (already tier-1-coded) subband contributions:
/// a bit-stuffed header (all blocks included in this single layer) followed by
/// the concatenated coded segments. An empty group yields an empty packet.
fn encode_one_packet(group: &[&EncodedBand]) -> Vec<u8> {
    let mut hw = HeaderWriter::new();
    if group.is_empty() {
        hw.put_bit(0); // empty packet (no code-blocks at this res/comp)
        return hw.finish();
    }
    hw.put_bit(1); // non-empty packet
    for band in group {
        // Inclusion tag-tree of a 1×1 grid: block included in this layer →
        // encode value 0 against threshold 1 as a single 1-bit.
        hw.put_bit(1);
        // Zero-bit-plane tag-tree (1×1): `zbp` zero-bits then a one-bit.
        for _ in 0..band.zbp {
            hw.put_bit(0);
        }
        hw.put_bit(1);
        // Number of new coding passes.
        hw.put_num_passes(band.passes);
        // Lblock increment: grow from 3 until it can express the length.
        let mut lblock = 3u32;
        loop {
            let bits = lblock + floor_log2(band.passes.max(1));
            if (band.data.len() as u32) < (1 << bits) {
                break;
            }
            lblock += 1;
        }
        for _ in 0..(lblock - 3) {
            hw.put_bit(1);
        }
        hw.put_bit(0);
        let bits = lblock + floor_log2(band.passes.max(1));
        hw.put_value(band.data.len() as u32, bits);
    }
    let mut out = hw.finish();
    for band in group {
        out.extend_from_slice(&band.data);
    }
    out
}

/// A subband's placement in the interleaved transform pyramid, its resolution
/// level (matching the decoder's resolution index) and tier-1 orientation/gain.
struct BandSpec {
    res: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    orient_kind: u8,
    gain: u32,
}

/// Compute the standard interleaved subband layout for an origin-(0,0)
/// `w×h` plane with `levels` decomposition levels, in resolution order
/// (LL at resolution 0, then resolutions 1..=levels each with HL, LH, HH).
fn subband_layout(w: usize, h: usize, levels: u32) -> Vec<BandSpec> {
    // Resolution dims: res r has dims ceil(w/2^(levels-r)).
    let dim = |size: usize, down: u32| -> usize { size.div_ceil(1 << down) };
    let mut specs = Vec::new();
    // LL at resolution 0.
    let llw = dim(w, levels);
    let llh = dim(h, levels);
    specs.push(BandSpec {
        res: 0,
        x0: 0,
        y0: 0,
        x1: llw,
        y1: llh,
        orient_kind: 0,
        gain: 0,
    });
    // Resolution r (1..=levels) carries the HL/LH/HH detail bands.
    for r in 1..=levels as usize {
        let rr = r as u32;
        // Resolution r dims.
        let rw = dim(w, levels - rr);
        let rh = dim(h, levels - rr);
        // Lower resolution (r-1) dims = the LL region size at this resolution.
        let lw = dim(w, levels - rr + 1);
        let lh = dim(h, levels - rr + 1);
        // HL: top-right (x in [lw, rw), y in [0, lh)).
        specs.push(BandSpec {
            res: r,
            x0: lw,
            y0: 0,
            x1: rw,
            y1: lh,
            orient_kind: 1,
            gain: 1,
        });
        // LH: bottom-left.
        specs.push(BandSpec {
            res: r,
            x0: 0,
            y0: lh,
            x1: lw,
            y1: rh,
            orient_kind: 0,
            gain: 1,
        });
        // HH: bottom-right.
        specs.push(BandSpec {
            res: r,
            x0: lw,
            y0: lh,
            x1: rw,
            y1: rh,
            orient_kind: 2,
            gain: 2,
        });
    }
    specs
}

fn band_gain(band_index: usize, _levels: u32) -> u32 {
    if band_index == 0 {
        return 0; // LL
    }
    match (band_index - 1) % 3 {
        0 => 1, // HL
        1 => 1, // LH
        _ => 2, // HH
    }
}

fn floor_log2(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        31 - v.leading_zeros()
    }
}

/// Bit-stuffed packet-header writer (the inverse of the decoder's reader).
struct HeaderWriter {
    cur: u8,
    nbits: u8,
    last_ff: bool,
    out: Vec<u8>,
}

impl HeaderWriter {
    fn new() -> Self {
        HeaderWriter {
            cur: 0,
            nbits: 0,
            last_ff: false,
            out: Vec::new(),
        }
    }

    fn put_bit(&mut self, bit: u8) {
        let cap = if self.last_ff { 7 } else { 8 };
        self.cur = (self.cur << 1) | (bit & 1);
        self.nbits += 1;
        if self.nbits == cap {
            self.flush_byte();
        }
    }

    fn flush_byte(&mut self) {
        // Left-justify the accumulated bits into a full byte.
        let cap = if self.last_ff { 7 } else { 8 };
        let byte = if cap == 7 {
            self.cur << 1 // top bit is the stuffed 0
        } else {
            self.cur
        };
        // For a stuffed byte the high bit must be 0; `cur` holds 7 bits so the
        // shift already places a 0 on top.
        let byte = if cap == 7 { byte & 0x7F } else { byte };
        self.out.push(byte);
        self.last_ff = byte == 0xFF;
        self.cur = 0;
        self.nbits = 0;
    }

    fn put_value(&mut self, v: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.put_bit(((v >> i) & 1) as u8);
        }
    }

    fn put_num_passes(&mut self, passes: u32) {
        match passes {
            1 => self.put_bit(0),
            2 => {
                self.put_bit(1);
                self.put_bit(0);
            }
            3..=5 => {
                self.put_bit(1);
                self.put_bit(1);
                self.put_value(passes - 3, 2);
            }
            6..=36 => {
                self.put_bit(1);
                self.put_bit(1);
                self.put_value(3, 2);
                self.put_value(passes - 6, 5);
            }
            _ => {
                self.put_bit(1);
                self.put_bit(1);
                self.put_value(3, 2);
                self.put_value(31, 5);
                self.put_value(passes - 37, 7);
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            // Pad remaining bits with zeros and flush.
            let cap = if self.last_ff { 7 } else { 8 };
            self.cur <<= cap - self.nbits;
            let byte = if cap == 7 {
                (self.cur << 1) & 0x7F
            } else {
                self.cur
            };
            self.out.push(byte);
        }
        self.out
    }
}

// ===========================================================================
// Tests.
// ===========================================================================

/// Helper: build the codestream, decode it, and return the per-component planes
/// of the decoded image.
fn roundtrip(enc: &Encoded) -> super::Image {
    let cs = enc.codestream();
    decode_to_image(&cs).expect("decode")
}

#[test]
fn e2e_single_component_no_dwt_reversible() {
    // NL=0: the LL band is the image; no DWT, no MCT. Isolates container + SIZ/
    // COD/QCD + tier-2 + tier-1 + level-shift. A 4×4 gradient.
    let w = 4u32;
    let h = 4u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..16).map(|i| (i * 16) as u8).collect();
    // Level-shifted samples (signed, what the codestream stores).
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded::single_tile(w, h, bit_depth, 1, 0, true, false, vec![comp]);
    let img = roundtrip(&enc);
    assert_eq!(img.width, 4);
    assert_eq!(img.height, 4);
    let got: Vec<u8> = img.planes[0].iter().map(|&v| v as u8).collect();
    assert_eq!(got, pixels, "decoded pixels must match the original");
}

#[test]
fn e2e_single_component_one_level_reversible() {
    // NL=1: exercises the inverse 5/3 DWT in the full pipeline. 8×8 image.
    let w = 8u32;
    let h = 8u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..64)
        .map(|i| {
            let x = i % 8;
            let y = i / 8;
            ((x * 16 + y * 8) % 240) as u8
        })
        .collect();
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded::single_tile(w, h, bit_depth, 1, 1, true, false, vec![comp]);
    let img = roundtrip(&enc);
    let got: Vec<u8> = img.planes[0].iter().map(|&v| v as u8).collect();
    assert_eq!(got, pixels, "5/3 one-level reconstruction must be exact");
}

#[test]
fn e2e_single_component_two_levels_reversible() {
    // NL=2: multi-resolution inverse 5/3 DWT.
    let w = 8u32;
    let h = 8u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..64).map(|i| ((i * 3) % 200 + 20) as u8).collect();
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded::single_tile(w, h, bit_depth, 1, 2, true, false, vec![comp]);
    let img = roundtrip(&enc);
    let got: Vec<u8> = img.planes[0].iter().map(|&v| v as u8).collect();
    assert_eq!(got, pixels, "5/3 two-level reconstruction must be exact");
}

#[test]
fn e2e_rgb_rct_reversible() {
    // Three components with the reversible RCT (MCT). 4×4. After forward RCT in
    // the encoder, the decoder's inverse RCT must recover the RGB pixels.
    let w = 4u32;
    let h = 4u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let r: Vec<u8> = (0..16).map(|i| (i * 15) as u8).collect();
    let g: Vec<u8> = (0..16).map(|i| (200 - i * 10) as u8).collect();
    let b: Vec<u8> = (0..16).map(|i| (i * 8 + 30) as u8).collect();
    // Forward RCT to Y/Db/Dr (level-shifted Y; Db/Dr are already signed diffs).
    let mut y = vec![0i32; 16];
    let mut db = vec![0i32; 16];
    let mut dr = vec![0i32; 16];
    for i in 0..16 {
        let (ri, gi, bi) = (r[i] as i32, g[i] as i32, b[i] as i32);
        let yy = (ri + 2 * gi + bi) >> 2;
        y[i] = yy - shift; // level-shift only the luma-like component
        db[i] = bi - gi;
        dr[i] = ri - gi;
    }
    let enc = Encoded::single_tile(w, h, bit_depth, 3, 0, true, true, vec![y, db, dr]);
    let img = roundtrip(&enc);
    let gr: Vec<u8> = img.planes[0].iter().map(|&v| v as u8).collect();
    let gg: Vec<u8> = img.planes[1].iter().map(|&v| v as u8).collect();
    let gb: Vec<u8> = img.planes[2].iter().map(|&v| v as u8).collect();
    assert_eq!(gr, r, "R channel");
    assert_eq!(gg, g, "G channel");
    assert_eq!(gb, b, "B channel");
}

#[test]
fn e2e_single_component_one_level_irreversible() {
    // NL=1, 9/7 irreversible path. Lossy: assert near-exact (±2) recovery.
    let w = 8u32;
    let h = 8u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..64).map(|i| ((i * 5) % 220 + 10) as u8).collect();
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded::single_tile(w, h, bit_depth, 1, 1, false, false, vec![comp]);
    let img = roundtrip(&enc);
    for (g, p) in img.planes[0].iter().zip(pixels.iter()) {
        let d = (*g - *p as i32).abs();
        assert!(d <= 2, "9/7 reconstruction off by {d} (got {g}, want {p})");
    }
}

#[test]
fn e2e_decodes_through_filter_dispatch() {
    // The whole thing routed through the public `/JPXDecode` filter entry.
    let w = 4u32;
    let h = 4u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..16).map(|i| (255 - i * 12) as u8).collect();
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded::single_tile(w, h, bit_depth, 1, 0, true, false, vec![comp]);
    let cs = enc.codestream();
    let samples = super::jpx_decode(&cs).expect("jpx_decode");
    assert_eq!(samples, pixels);
}

#[test]
fn e2e_multi_tile_reversible() {
    // A 2×2 tile grid over an 8×8 image (4×4 tiles): exercises the codestream's
    // multi-SOT tile loop and the decoder's per-tile reconstruction + scatter.
    let w = 8u32;
    let h = 8u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..64)
        .map(|i| {
            let x = i % 8;
            let y = i / 8;
            ((x * 20 + y * 12) % 250) as u8
        })
        .collect();
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded {
        w,
        h,
        bit_depth,
        ncomp: 1,
        levels: 1,
        reversible: true,
        mct: false,
        tiles_x: 2,
        tiles_y: 2,
        comp_samples: vec![comp],
    };
    let img = roundtrip(&enc);
    let got: Vec<u8> = img.planes[0].iter().map(|&v| v as u8).collect();
    assert_eq!(got, pixels, "multi-tile 5/3 reconstruction must be exact");
}

#[test]
fn e2e_jp2_box_wrapper() {
    // The same codestream wrapped in a minimal JP2 box structure must decode
    // identically (the container layer locates the jp2c codestream).
    let w = 4u32;
    let h = 4u32;
    let bit_depth = 8u32;
    let shift = 1i32 << (bit_depth - 1);
    let pixels: Vec<u8> = (0..16).map(|i| (i * 16) as u8).collect();
    let comp: Vec<i32> = pixels.iter().map(|&p| p as i32 - shift).collect();
    let enc = Encoded::single_tile(w, h, bit_depth, 1, 0, true, false, vec![comp]);
    let cs = enc.codestream();
    // Wrap: signature box `jP  `, then a `jp2c` box carrying the codestream.
    let mut jp2 = Vec::new();
    jp2.extend_from_slice(&12u32.to_be_bytes());
    jp2.extend_from_slice(b"jP  ");
    jp2.extend_from_slice(&[0x0D, 0x0A, 0x87, 0x0A]);
    jp2.extend_from_slice(&((8 + cs.len()) as u32).to_be_bytes());
    jp2.extend_from_slice(b"jp2c");
    jp2.extend_from_slice(&cs);
    let samples = super::jpx_decode(&jp2).expect("jpx_decode (JP2 wrapper)");
    assert_eq!(samples, pixels);
}
