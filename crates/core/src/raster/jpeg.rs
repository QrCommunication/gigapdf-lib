//! JPEG encoder (baseline) + decoder (baseline, **progressive**, and
//! **arithmetic-coded**) — pure std, zero dependency.
//!
//! The **encoder** emits full-resolution 4:4:4 baseline JPEGs (no chroma
//! subsampling) using the ISO/IEC 10918-1 Annex K example quantization and
//! Huffman tables — enough to re-encode rendered previews/thumbnails. The
//! **decoder** handles baseline (SOF0), progressive (SOF2), and **arithmetic**
//! (SOF9 sequential, SOF10 progressive) streams — including
//! successive-approximation refinement, EOB runs, chroma subsampling
//! (nearest-neighbour upsample), and restart markers — the native replacement
//! for a third-party image library's JPEG path. Arithmetic decoding uses the
//! ISO/IEC 10918-1 Annex MQ/QM-coder (identical to the ITU-T T.82/JBIG
//! arithmetic coder, same `Qe` table) with the §F.1.4 DC/AC context models and
//! optional DAC-marker conditioning. **Lossless** SOF3/SOF11 (spatial
//! predictor, not DCT) and 12-bit extended-sequential-Huffman SOF1 remain
//! unsupported and decode to `None` (the caller skips the image rather than
//! blanking the page). Orthonormal float DCT-II / DCT-III (forward/inverse are
//! an exact pair).

use std::collections::HashMap;
use std::f32::consts::PI;

/// Natural pixel index of each coefficient in zig-zag scan order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const STD_LUMA_QUANT: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

const STD_CHROMA_QUANT: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

// ── Standard Huffman tables (Annex K.3): (counts per code length 1..=16, values).
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// Canonical-Huffman code per symbol value: `(code, bit length)`.
fn build_codes(bits: &[u8; 16], vals: &[u8]) -> HashMap<u8, (u16, u8)> {
    let mut map = HashMap::new();
    let mut code: u16 = 0;
    let mut k = 0;
    for (len_minus1, &count) in bits.iter().enumerate() {
        let len = len_minus1 as u8 + 1;
        for _ in 0..count {
            map.insert(vals[k], (code, len));
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    map
}

/// Reverse of [`build_codes`] for decoding: `(bit length, code) -> value`.
fn build_decode(bits: &[u8; 16], vals: &[u8]) -> HashMap<(u8, u16), u8> {
    let mut map = HashMap::new();
    let mut code: u16 = 0;
    let mut k = 0;
    for (len_minus1, &count) in bits.iter().enumerate() {
        let len = len_minus1 as u8 + 1;
        for _ in 0..count {
            map.insert((len, code), vals[k]);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    map
}

/// Scale a base quantization table by `quality` (1..=100; 75 ≈ the common default).
fn scaled_quant(base: &[u16; 64], quality: u32) -> [u16; 64] {
    let q = quality.clamp(1, 100);
    let s = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    for (o, &b) in out.iter_mut().zip(base.iter()) {
        *o = (((b as u32 * s + 50) / 100).clamp(1, 255)) as u16;
    }
    out
}

// ── DCT (orthonormal; forward DCT-II and inverse DCT-III are an exact pair) ────

fn alpha(u: usize) -> f32 {
    if u == 0 {
        (1.0f32 / 8.0).sqrt()
    } else {
        (2.0f32 / 8.0).sqrt()
    }
}

fn dct_ii(inp: &[f32; 8]) -> [f32; 8] {
    let mut out = [0f32; 8];
    for (u, o) in out.iter_mut().enumerate() {
        let mut s = 0.0;
        for (x, &v) in inp.iter().enumerate() {
            s += v * (((2 * x + 1) as f32) * (u as f32) * PI / 16.0).cos();
        }
        *o = alpha(u) * s;
    }
    out
}

fn dct_iii(inp: &[f32; 8]) -> [f32; 8] {
    let mut out = [0f32; 8];
    for (x, o) in out.iter_mut().enumerate() {
        let mut s = 0.0;
        for (u, &v) in inp.iter().enumerate() {
            s += alpha(u) * v * (((2 * x + 1) as f32) * (u as f32) * PI / 16.0).cos();
        }
        *o = s;
    }
    out
}

/// Apply a 1-D transform to all rows then all columns of an 8×8 block.
fn transform_2d(block: &mut [f32; 64], f: impl Fn(&[f32; 8]) -> [f32; 8]) {
    let mut row = [0f32; 8];
    for r in 0..8 {
        row.copy_from_slice(&block[r * 8..r * 8 + 8]);
        let t = f(&row);
        block[r * 8..r * 8 + 8].copy_from_slice(&t);
    }
    let mut col = [0f32; 8];
    for c in 0..8 {
        for r in 0..8 {
            col[r] = block[r * 8 + c];
        }
        let t = f(&col);
        for r in 0..8 {
            block[r * 8 + c] = t[r];
        }
    }
}

// ── Bit writer (MSB-first, 0xFF byte-stuffed) ─────────────────────────────────

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }
    fn put(&mut self, code: u16, len: u8) {
        self.acc |= (code as u32) << (32 - self.nbits - len as u32);
        self.nbits += len as u32;
        while self.nbits >= 8 {
            let byte = (self.acc >> 24) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00); // stuff
            }
            self.acc <<= 8;
            self.nbits -= 8;
        }
    }
    fn flush(&mut self) {
        if self.nbits > 0 {
            // Pad the final partial byte with 1-bits (JPEG convention, ITU-T
            // T.81 §F.1.2.3). Only the `pad` free low bits may be set: a fixed
            // 7-bit `0x7F` would, for any `nbits > 1`, bleed its extra 1-bits
            // into the already-written code field via the `|=` in `put`,
            // corrupting the final Huffman code.
            let pad = (8 - self.nbits) as u8; // 1..=7
            self.put((1u16 << pad) - 1, pad);
        }
    }
}

/// The magnitude category (number of significant bits) of `v`, and its
/// `len`-bit JPEG amplitude code (negative values use the one's-complement low
/// bits).
fn magnitude(v: i32) -> (u8, u16) {
    if v == 0 {
        return (0, 0);
    }
    let a = v.unsigned_abs();
    let cat = (32 - a.leading_zeros()) as u8;
    // Negative amplitudes use the one's-complement low bits.
    let code = if v > 0 { a } else { a ^ ((1u32 << cat) - 1) };
    (cat, (code & 0xFFFF) as u16)
}

struct Tables {
    dc_luma: HashMap<u8, (u16, u8)>,
    ac_luma: HashMap<u8, (u16, u8)>,
    dc_chroma: HashMap<u8, (u16, u8)>,
    ac_chroma: HashMap<u8, (u16, u8)>,
}

#[allow(clippy::too_many_arguments)]
fn encode_block(
    w: &mut BitWriter,
    coeffs: &[i32; 64],
    prev_dc: &mut i32,
    dc_tab: &HashMap<u8, (u16, u8)>,
    ac_tab: &HashMap<u8, (u16, u8)>,
) {
    // DC: difference from the previous block's DC.
    let diff = coeffs[0] - *prev_dc;
    *prev_dc = coeffs[0];
    let (cat, bits) = magnitude(diff);
    let (code, len) = dc_tab[&cat];
    w.put(code, len);
    if cat > 0 {
        w.put(bits, cat);
    }
    // AC: run-length of zeros + nonzero magnitude, in zig-zag order.
    let mut run = 0u8;
    for i in 1..64 {
        let v = coeffs[ZIGZAG[i]];
        if v == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            let (zc, zl) = ac_tab[&0xF0]; // ZRL
            w.put(zc, zl);
            run -= 16;
        }
        let (cat, bits) = magnitude(v);
        let sym = (run << 4) | cat;
        let (code, len) = ac_tab[&sym];
        w.put(code, len);
        w.put(bits, cat);
        run = 0;
    }
    if run > 0 {
        let (code, len) = ac_tab[&0x00]; // EOB
        w.put(code, len);
    }
}

/// Encode raw RGBA pixels (`width*height*4`) to a baseline JPEG at `quality`
/// (1..=100). Alpha is ignored (composited onto white). Empty `Vec` on a bad
/// input.
pub fn encode_jpeg(width: u32, height: u32, rgba: &[u8], quality: u32) -> Vec<u8> {
    if width == 0 || height == 0 || rgba.len() != (width as usize * height as usize * 4) {
        return Vec::new();
    }
    let lq = scaled_quant(&STD_LUMA_QUANT, quality);
    let cq = scaled_quant(&STD_CHROMA_QUANT, quality);
    let tables = Tables {
        dc_luma: build_codes(&DC_LUMA_BITS, &DC_LUMA_VALS),
        ac_luma: build_codes(&AC_LUMA_BITS, &AC_LUMA_VALS),
        dc_chroma: build_codes(&DC_CHROMA_BITS, &DC_CHROMA_VALS),
        ac_chroma: build_codes(&AC_CHROMA_BITS, &AC_CHROMA_VALS),
    };

    let w = width as usize;
    let h = height as usize;
    // Sample a pixel with edge replication for partial blocks (composite on white).
    let sample = |x: usize, y: usize| -> (f32, f32, f32) {
        let xx = x.min(w - 1);
        let yy = y.min(h - 1);
        let p = (yy * w + xx) * 4;
        let a = rgba[p + 3] as f32 / 255.0;
        let r = rgba[p] as f32 * a + 255.0 * (1.0 - a);
        let g = rgba[p + 1] as f32 * a + 255.0 * (1.0 - a);
        let b = rgba[p + 2] as f32 * a + 255.0 * (1.0 - a);
        (r, g, b)
    };

    let mut bw = BitWriter::new();
    let (mut dc_y, mut dc_cb, mut dc_cr) = (0i32, 0i32, 0i32);
    let bx = w.div_ceil(8);
    let by = h.div_ceil(8);
    for byi in 0..by {
        for bxi in 0..bx {
            let mut yb = [0f32; 64];
            let mut cbb = [0f32; 64];
            let mut crb = [0f32; 64];
            for r in 0..8 {
                for c in 0..8 {
                    let (rr, gg, bb) = sample(bxi * 8 + c, byi * 8 + r);
                    // BT.601 RGB→YCbCr, then level-shift by −128.
                    let yv = 0.299 * rr + 0.587 * gg + 0.114 * bb;
                    let cb = -0.168_736 * rr - 0.331_264 * gg + 0.5 * bb + 128.0;
                    let cr = 0.5 * rr - 0.418_688 * gg - 0.081_312 * bb + 128.0;
                    yb[r * 8 + c] = yv - 128.0;
                    cbb[r * 8 + c] = cb - 128.0;
                    crb[r * 8 + c] = cr - 128.0;
                }
            }
            for (blk, q, dc, dct, act) in [
                (&mut yb, &lq, &mut dc_y, &tables.dc_luma, &tables.ac_luma),
                (
                    &mut cbb,
                    &cq,
                    &mut dc_cb,
                    &tables.dc_chroma,
                    &tables.ac_chroma,
                ),
                (
                    &mut crb,
                    &cq,
                    &mut dc_cr,
                    &tables.dc_chroma,
                    &tables.ac_chroma,
                ),
            ] {
                transform_2d(blk, dct_ii);
                let mut coeffs = [0i32; 64];
                for i in 0..64 {
                    coeffs[i] = (blk[i] / q[i] as f32).round() as i32;
                }
                encode_block(&mut bw, &coeffs, dc, dct, act);
            }
        }
    }
    bw.flush();

    assemble(width, height, &lq, &cq, &bw.out)
}

/// Build the JFIF container around the entropy-coded scan.
fn assemble(width: u32, height: u32, lq: &[u16; 64], cq: &[u16; 64], scan: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(scan.len() + 640);
    out.extend_from_slice(&[0xFF, 0xD8]); // SOI
                                          // APP0 / JFIF.
    out.extend_from_slice(&[
        0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x00,
    ]);
    // DQT (two 8-bit tables, in zig-zag order).
    for (id, q) in [(0u8, lq), (1u8, cq)] {
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, id]);
        for &z in &ZIGZAG {
            out.push(q[z] as u8);
        }
    }
    // SOF0 — baseline, 3 components, all 1×1 sampling (4:4:4).
    let (hb, wb) = (height.to_be_bytes(), width.to_be_bytes());
    out.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x11, 0x08, hb[2], hb[3], wb[2], wb[3], 0x03,
    ]);
    out.extend_from_slice(&[0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
    // DHT — four tables.
    for (class_id, bits, vals) in [
        (0x00u8, &DC_LUMA_BITS[..], &DC_LUMA_VALS[..]),
        (0x10, &AC_LUMA_BITS[..], &AC_LUMA_VALS[..]),
        (0x01, &DC_CHROMA_BITS[..], &DC_CHROMA_VALS[..]),
        (0x11, &AC_CHROMA_BITS[..], &AC_CHROMA_VALS[..]),
    ] {
        let len = 19 + vals.len();
        out.extend_from_slice(&[0xFF, 0xC4, (len >> 8) as u8, (len & 0xFF) as u8, class_id]);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    // SOS.
    out.extend_from_slice(&[
        0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3F, 0x00,
    ]);
    out.extend_from_slice(scan);
    out.extend_from_slice(&[0xFF, 0xD9]); // EOI
    out
}

// ── Decoder ───────────────────────────────────────────────────────────────────

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u32,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader {
            data,
            pos: 0,
            acc: 0,
            nbits: 0,
        }
    }
    fn bit(&mut self) -> u32 {
        if self.nbits == 0 {
            let mut byte = *self.data.get(self.pos).unwrap_or(&0);
            self.pos += 1;
            if byte == 0xFF {
                // Skip a stuffed 0x00; a real marker ends the scan (treat as 0s).
                match self.data.get(self.pos) {
                    Some(0x00) => self.pos += 1,
                    Some(m) if (0xD0..=0xD7).contains(m) => {
                        self.pos += 1; // restart marker — skip, continue
                        byte = *self.data.get(self.pos).unwrap_or(&0);
                        self.pos += 1;
                    }
                    _ => byte = 0,
                }
            }
            self.acc = byte as u32;
            self.nbits = 8;
        }
        self.nbits -= 1;
        (self.acc >> self.nbits) & 1
    }
    fn receive(&mut self, n: u8) -> i32 {
        let mut v = 0i32;
        for _ in 0..n {
            v = (v << 1) | self.bit() as i32;
        }
        v
    }
    /// Extend a `cat`-bit JPEG amplitude to a signed value.
    fn receive_extend(&mut self, cat: u8) -> i32 {
        if cat == 0 {
            return 0;
        }
        let v = self.receive(cat);
        if v < (1 << (cat - 1)) {
            v - (1 << cat) + 1
        } else {
            v
        }
    }
    fn decode_huff(&mut self, table: &HashMap<(u8, u16), u8>) -> Option<u8> {
        let mut code: u16 = 0;
        for len in 1..=16u8 {
            code = (code << 1) | self.bit() as u16;
            if let Some(&v) = table.get(&(len, code)) {
                return Some(v);
            }
        }
        None
    }
}

struct Component {
    id: u8,
    h: u8,
    v: u8,
    quant: usize,
    dc_tab: usize,
    ac_tab: usize,
    pred: i32,
}

/// Decode a baseline **or progressive** JPEG into `(width, height, rgba)`.
/// Returns `None` on an unsupported (arithmetic-coded) or malformed stream.
/// Chroma components are upsampled by nearest-neighbour to the luma grid.
#[allow(clippy::type_complexity)]
pub fn decode_jpeg(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut pos = 2;
    let mut quant: [[u16; 64]; 4] = [[0; 64]; 4];
    let mut dc_tabs: Vec<HashMap<(u8, u16), u8>> = vec![HashMap::new(); 4];
    let mut ac_tabs: Vec<HashMap<(u8, u16), u8>> = vec![HashMap::new(); 4];
    let mut width = 0u32;
    let mut height = 0u32;
    let mut comps: Vec<Component> = Vec::new();
    let mut restart_interval = 0usize;
    // Progressive (SOF2) decode state, allocated when SOF2 is seen.
    let mut progressive: Option<Progressive> = None;
    // True once an arithmetic SOF (SOF9 sequential, SOF10 progressive) is seen;
    // SOS then dispatches to the MQ decoder instead of the Huffman path.
    let mut arithmetic = false;
    // DAC arithmetic conditioning (defaults applied when no DAC marker): per
    // table-id `(L, U)` for DC (the 5-zone classification bounds) and `Kx` for
    // AC (the magnitude-split context threshold).
    let mut arith_dc: [ArithDcCond; 4] = [ArithDcCond::default(); 4];
    let mut arith_ac: [ArithAcCond; 4] = [ArithAcCond::default(); 4];

    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        pos += 2;
        match marker {
            0xD8 | 0xD9 => continue,
            0xC0 | 0xC2 | 0xC9 | 0xCA => {
                // SOF0 (baseline Huffman), SOF2 (progressive Huffman),
                // SOF9 (extended-sequential arithmetic) or SOF10 (progressive
                // arithmetic) — all share the same frame-header layout.
                let l = be16(data, pos) as usize;
                height = be16(data, pos + 3) as u32;
                width = be16(data, pos + 5) as u32;
                let n = *data.get(pos + 7).unwrap_or(&0) as usize;
                // Bounds-check the component descriptors (3 bytes each); a
                // truncated header aborts the decode rather than panicking.
                if pos + 8 + n * 3 > data.len() {
                    return None;
                }
                for i in 0..n {
                    let o = pos + 8 + i * 3;
                    comps.push(Component {
                        id: data[o],
                        h: data[o + 1] >> 4,
                        v: data[o + 1] & 0x0F,
                        quant: data[o + 2] as usize,
                        dc_tab: 0,
                        ac_tab: 0,
                        pred: 0,
                    });
                }
                arithmetic = marker == 0xC9 || marker == 0xCA;
                if marker == 0xC2 || marker == 0xCA {
                    // Progressive (Huffman or arithmetic): allocate the shared
                    // coefficient store. A malformed header aborts the decode.
                    progressive = Some(Progressive::new(width, height, &comps)?);
                }
                pos += l;
            }
            // SOF1 (12-bit extended-sequential Huffman) and lossless SOF3/SOF11
            // (spatial predictor, not DCT) remain unsupported.
            0xC1 | 0xC3 | 0xCB => return None,
            0xCC => {
                // DAC — arithmetic conditioning tables (one byte `Tc<<4|Tb`
                // selector + one `Cs` conditioning byte per entry).
                let l = be16(data, pos) as usize;
                let end = pos + l;
                let mut o = pos + 2;
                while o + 1 < end {
                    let tc_tb = data[o];
                    let cs = data[o + 1];
                    o += 2;
                    let idx = (tc_tb & 0x0F) as usize;
                    if idx >= 4 {
                        continue;
                    }
                    if tc_tb & 0x10 == 0 {
                        // DC: low nibble = L (lower bound), high nibble = U.
                        arith_dc[idx] = ArithDcCond {
                            l: cs & 0x0F,
                            u: cs >> 4,
                        };
                    } else {
                        // AC: Kx threshold (1..=63).
                        arith_ac[idx] = ArithAcCond { kx: cs };
                    }
                }
                pos = end;
            }
            0xC4 => {
                let l = be16(data, pos) as usize;
                let end = pos + l;
                let mut o = pos + 2;
                while o < end {
                    let tc_th = data[o];
                    o += 1;
                    let mut bits = [0u8; 16];
                    bits.copy_from_slice(&data[o..o + 16]);
                    o += 16;
                    let count: usize = bits.iter().map(|&b| b as usize).sum();
                    let vals = &data[o..o + count];
                    o += count;
                    let table = build_decode(&bits, vals);
                    let idx = (tc_th & 0x0F) as usize;
                    if tc_th & 0x10 == 0 {
                        dc_tabs[idx] = table;
                    } else {
                        ac_tabs[idx] = table;
                    }
                }
                pos = end;
            }
            0xDB => {
                let l = be16(data, pos) as usize;
                let end = pos + l;
                let mut o = pos + 2;
                while o < end {
                    let pq_tq = data[o];
                    o += 1;
                    let id = (pq_tq & 0x0F) as usize;
                    let sixteen = pq_tq >> 4 != 0;
                    let mut t = [0u16; 64];
                    for &z in &ZIGZAG {
                        if sixteen {
                            t[z] = be16(data, o);
                            o += 2;
                        } else {
                            t[z] = data[o] as u16;
                            o += 1;
                        }
                    }
                    if id < 4 {
                        quant[id] = t;
                    }
                }
                pos = end;
            }
            0xDD => {
                // DRI — restart interval (in MCUs).
                let l = be16(data, pos) as usize;
                restart_interval = be16(data, pos + 2) as usize;
                pos += l;
            }
            0xDA => {
                let l = be16(data, pos) as usize;
                let ns = data[pos + 2] as usize;
                // Scan component selectors + table assignments.
                let mut scan_comps: Vec<usize> = Vec::with_capacity(ns);
                for i in 0..ns {
                    let o = pos + 3 + i * 2;
                    let cid = data[o];
                    let td_ta = data[o + 1];
                    if let Some(ci) = comps.iter().position(|c| c.id == cid) {
                        comps[ci].dc_tab = (td_ta >> 4) as usize;
                        comps[ci].ac_tab = (td_ta & 0x0F) as usize;
                        scan_comps.push(ci);
                    }
                }
                // Spectral selection + successive approximation (after the
                // component list): Ss, Se, Ah<<4 | Al.
                let sp = pos + 3 + ns * 2;
                let ss = *data.get(sp).unwrap_or(&0);
                let se = *data.get(sp + 1).unwrap_or(&63);
                let ah_al = *data.get(sp + 2).unwrap_or(&0);
                pos += l;
                if arithmetic {
                    if let Some(prog) = progressive.as_mut() {
                        // SOF10: arithmetic progressive — decode this scan into
                        // the coefficient store via the MQ decoder, then
                        // continue to the next marker.
                        pos = prog.decode_scan_arith(
                            data,
                            pos,
                            &comps,
                            &arith_dc,
                            &arith_ac,
                            &scan_comps,
                            ss,
                            se,
                            ah_al >> 4,
                            ah_al & 0x0F,
                            restart_interval,
                        )?;
                        continue;
                    }
                    // SOF9: arithmetic sequential — one interleaved scan decodes
                    // the whole image.
                    return decode_scan_arith(
                        data, pos, width, height, &mut comps, &quant, &arith_dc, &arith_ac,
                        restart_interval,
                    );
                }
                if let Some(prog) = progressive.as_mut() {
                    // Progressive: decode this scan into the coefficient store,
                    // then continue to the next marker.
                    pos = prog.decode_scan(
                        data,
                        pos,
                        &comps,
                        &dc_tabs,
                        &ac_tabs,
                        &scan_comps,
                        ss,
                        se,
                        ah_al >> 4,
                        ah_al & 0x0F,
                        restart_interval,
                    )?;
                    continue;
                }
                // Baseline: a single interleaved scan decodes the whole image.
                return decode_scan(
                    data, pos, width, height, &mut comps, &quant, &dc_tabs, &ac_tabs,
                );
            }
            0xD0..=0xD7 => continue, // standalone restart (shouldn't appear here)
            _ => {
                let l = be16(data, pos) as usize;
                pos += l;
            }
        }
    }
    // Progressive streams finalize after all scans (no early return at SOS).
    if let Some(prog) = progressive {
        return Some(prog.finish(&comps, &quant));
    }
    None
}

/// Place one already-IDCT'd, level-shifted 8×8 block (`block[i] + 128` gives the
/// sample) into `plane`, replicating each sample over its `sx`×`sy` footprint on
/// the luma grid (nearest-neighbour chroma upsample). `(bcol, brow)` are the
/// block's position in this component's own 8×8 grid.
#[allow(clippy::too_many_arguments)]
fn place_block(
    plane: &mut [f32],
    block: &[f32; 64],
    bcol: usize,
    brow: usize,
    sx: usize,
    sy: usize,
    width: usize,
    height: usize,
) {
    let ox = bcol * 8;
    let oy = brow * 8;
    for r in 0..8 {
        for col in 0..8 {
            let val = block[r * 8 + col] + 128.0;
            for dy in 0..sy {
                for dx in 0..sx {
                    let px = (ox + col) * sx + dx;
                    let py = (oy + r) * sy + dy;
                    if px < width && py < height {
                        plane[py * width + px] = val;
                    }
                }
            }
        }
    }
}

/// Convert per-component full-resolution `planes` (already upsampled to
/// `width`×`height`) to RGBA: YCbCr→RGB for 3+ components, grayscale otherwise.
fn planes_to_rgba(planes: &[Vec<f32>], width: usize, height: usize) -> Vec<u8> {
    let n = width * height;
    let mut out = vec![0u8; n * 4];
    let three = planes.len() >= 3;
    for i in 0..n {
        let (r, g, b) = if three {
            let y = planes[0][i];
            let cb = planes[1][i] - 128.0;
            let cr = planes[2][i] - 128.0;
            (
                y + 1.402 * cr,
                y - 0.344_136 * cb - 0.714_136 * cr,
                y + 1.772 * cb,
            )
        } else {
            let y = planes[0][i];
            (y, y, y)
        };
        out[i * 4] = r.round().clamp(0.0, 255.0) as u8;
        out[i * 4 + 1] = g.round().clamp(0.0, 255.0) as u8;
        out[i * 4 + 2] = b.round().clamp(0.0, 255.0) as u8;
        out[i * 4 + 3] = 255;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    data: &[u8],
    start: usize,
    width: u32,
    height: u32,
    comps: &mut [Component],
    quant: &[[u16; 64]; 4],
    dc_tabs: &[HashMap<(u8, u16), u8>],
    ac_tabs: &[HashMap<(u8, u16), u8>],
) -> Option<(u32, u32, Vec<u8>)> {
    if width == 0 || height == 0 || comps.is_empty() {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    let hmax = comps.iter().map(|c| c.h).max()?.max(1) as usize;
    let vmax = comps.iter().map(|c| c.v).max()?.max(1) as usize;
    let mcus_x = w.div_ceil(8 * hmax);
    let mcus_y = h.div_ceil(8 * vmax);

    // Per-component full-resolution plane (already upsampled to width×height).
    let mut planes: Vec<Vec<f32>> = comps.iter().map(|_| vec![0f32; w * h]).collect();

    let mut br = BitReader::new(&data[start..]);
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            for (ci, c) in comps.iter_mut().enumerate() {
                let (ch, cv) = (c.h as usize, c.v as usize);
                let sx = hmax / ch;
                let sy = vmax / cv;
                for by in 0..cv {
                    for bx in 0..ch {
                        let mut block = [0f32; 64];
                        decode_block(&mut br, c, quant, dc_tabs, ac_tabs, &mut block)?;
                        place_block(
                            &mut planes[ci],
                            &block,
                            mx * ch + bx,
                            my * cv + by,
                            sx,
                            sy,
                            w,
                            h,
                        );
                    }
                }
            }
        }
    }

    Some((width, height, planes_to_rgba(&planes, w, h)))
}

fn decode_block(
    br: &mut BitReader,
    c: &mut Component,
    quant: &[[u16; 64]; 4],
    dc_tabs: &[HashMap<(u8, u16), u8>],
    ac_tabs: &[HashMap<(u8, u16), u8>],
    block: &mut [f32; 64],
) -> Option<()> {
    let q = &quant[c.quant.min(3)];
    let mut coeffs = [0f32; 64];
    // DC.
    let t = br.decode_huff(&dc_tabs[c.dc_tab.min(3)])?;
    let diff = br.receive_extend(t);
    c.pred += diff;
    coeffs[0] = c.pred as f32 * q[0] as f32;
    // AC.
    let mut k = 1;
    while k < 64 {
        let rs = br.decode_huff(&ac_tabs[c.ac_tab.min(3)])?;
        let run = rs >> 4;
        let size = rs & 0x0F;
        if size == 0 {
            if run == 0x0F {
                k += 16; // ZRL
                continue;
            }
            break; // EOB
        }
        k += run as usize;
        if k >= 64 {
            break;
        }
        let val = br.receive_extend(size);
        let z = ZIGZAG[k];
        coeffs[z] = val as f32 * q[z] as f32;
        k += 1;
    }
    block.copy_from_slice(&coeffs);
    transform_2d(block, dct_iii);
    Some(())
}

// ── Progressive (SOF2) decoding (ISO/IEC 10918-1 Annex G) ─────────────────────
//
// A progressive JPEG ships its coefficients across several scans, each refining
// a spectral band (DC, or a run of AC coefficients) by adding bits. We keep one
// quantized-coefficient array per component (in zig-zag order), decode every
// scan into it, then dequantize + IDCT once at the end. Successive-approximation
// (bit-plane) refinement is supported. Restart markers are honoured.

/// A bit reader over the whole stream that reports its byte position, aligns at
/// restart markers, and treats a real (non-stuffed, non-restart) marker as the
/// end of the entropy-coded segment (returning 0 bits thereafter).
struct ProgReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u32,
    nbits: u32,
    /// Set once a terminating marker is reached; further reads yield 0.
    at_marker: bool,
}

impl<'a> ProgReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> ProgReader<'a> {
        ProgReader {
            data,
            pos,
            acc: 0,
            nbits: 0,
            at_marker: false,
        }
    }

    fn bit(&mut self) -> u32 {
        if self.nbits == 0 {
            if self.at_marker {
                return 0;
            }
            let byte = *self.data.get(self.pos).unwrap_or(&0xFF);
            if byte == 0xFF {
                match self.data.get(self.pos + 1) {
                    Some(0x00) => self.pos += 2, // stuffed byte
                    _ => {
                        // A real marker terminates the entropy segment.
                        self.at_marker = true;
                        return 0;
                    }
                }
            } else {
                self.pos += 1;
            }
            self.acc = byte as u32;
            self.nbits = 8;
        }
        self.nbits -= 1;
        (self.acc >> self.nbits) & 1
    }

    fn receive(&mut self, n: u8) -> i32 {
        let mut v = 0i32;
        for _ in 0..n {
            v = (v << 1) | self.bit() as i32;
        }
        v
    }

    fn receive_extend(&mut self, cat: u8) -> i32 {
        if cat == 0 {
            return 0;
        }
        let v = self.receive(cat);
        if v < (1 << (cat - 1)) {
            v - (1 << cat) + 1
        } else {
            v
        }
    }

    fn decode_huff(&mut self, table: &HashMap<(u8, u16), u8>) -> Option<u8> {
        let mut code: u16 = 0;
        for len in 1..=16u8 {
            code = (code << 1) | self.bit() as u16;
            if let Some(&v) = table.get(&(len, code)) {
                return Some(v);
            }
        }
        None
    }

    /// Discard buffered bits and skip a following restart marker (`FF Dn`),
    /// re-syncing for the next restart interval.
    fn restart(&mut self) {
        self.nbits = 0;
        self.acc = 0;
        self.at_marker = false;
        // Skip any pad up to the marker, then the two marker bytes.
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xFF {
                let m = self.data[self.pos + 1];
                if (0xD0..=0xD7).contains(&m) {
                    self.pos += 2;
                    return;
                }
                if m == 0x00 {
                    self.pos += 2; // stuffed, keep scanning
                    continue;
                }
                return; // some other marker — leave it for the caller
            }
            self.pos += 1;
        }
    }

    /// Advance `pos` to the next marker so the caller's marker loop resumes
    /// correctly after the entropy-coded segment (skips stuffed `FF00` and
    /// restart markers).
    fn seek_next_marker(&mut self) -> usize {
        // Flush partial byte first.
        self.nbits = 0;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xFF {
                let m = self.data[self.pos + 1];
                if m == 0x00 || (0xD0..=0xD7).contains(&m) {
                    self.pos += 2; // stuffed byte or restart — part of the scan
                    continue;
                }
                return self.pos; // a genuine marker
            }
            self.pos += 1;
        }
        self.pos
    }
}

/// One component's progressive coefficient grid.
struct ProgComp {
    /// Horizontal/vertical sampling factors.
    h: usize,
    v: usize,
    /// Blocks across / down for this component (its own grid).
    bx: usize,
    by: usize,
    quant: usize,
    /// `bx * by` blocks, each 64 quantized coefficients in **zig-zag** order.
    coeffs: Vec<[i32; 64]>,
}

/// Whole-image progressive decode state.
struct Progressive {
    width: usize,
    height: usize,
    hmax: usize,
    vmax: usize,
    /// MCUs across / down (interleaved DC scans iterate this grid).
    mcus_x: usize,
    mcus_y: usize,
    comps: Vec<ProgComp>,
    /// Carried EOB run between AC blocks within a scan.
    eobrun: u32,
}

impl Progressive {
    fn new(width: u32, height: u32, comps: &[Component]) -> Option<Progressive> {
        if width == 0 || height == 0 || comps.is_empty() {
            return None;
        }
        let (w, h) = (width as usize, height as usize);
        let hmax = comps.iter().map(|c| c.h.max(1) as usize).max()?;
        let vmax = comps.iter().map(|c| c.v.max(1) as usize).max()?;
        let mcus_x = w.div_ceil(8 * hmax);
        let mcus_y = h.div_ceil(8 * vmax);
        let mut pcomps = Vec::with_capacity(comps.len());
        for c in comps {
            let ch = c.h.max(1) as usize;
            let cv = c.v.max(1) as usize;
            // Each component's block grid covers all MCUs at its sampling.
            let bx = mcus_x * ch;
            let by = mcus_y * cv;
            // Guard against absurd allocations from a corrupt header.
            let blocks = bx.checked_mul(by)?;
            if blocks > 64 * 1024 * 1024 {
                return None;
            }
            pcomps.push(ProgComp {
                h: ch,
                v: cv,
                bx,
                by,
                quant: c.quant,
                coeffs: vec![[0i32; 64]; blocks],
            });
        }
        Some(Progressive {
            width: w,
            height: h,
            hmax,
            vmax,
            mcus_x,
            mcus_y,
            comps: pcomps,
            eobrun: 0,
        })
    }

    /// Decode one scan of entropy-coded data starting at `start`, mutating the
    /// coefficient store. Returns the byte position of the next marker.
    #[allow(clippy::too_many_arguments)]
    fn decode_scan(
        &mut self,
        data: &[u8],
        start: usize,
        comps: &[Component],
        dc_tabs: &[HashMap<(u8, u16), u8>],
        ac_tabs: &[HashMap<(u8, u16), u8>],
        scan_comps: &[usize],
        ss: u8,
        se: u8,
        ah: u8,
        al: u8,
        restart_interval: usize,
    ) -> Option<usize> {
        if scan_comps.is_empty() {
            return Some(start);
        }
        let mut rd = ProgReader::new(data, start);
        self.eobrun = 0;
        // Per-component DC predictors for this scan.
        let mut preds = vec![0i32; self.comps.len()];

        if ss == 0 {
            // DC scan — may be interleaved over several components (MCU order).
            // Build the per-MCU block list once.
            let interleaved = scan_comps.len() > 1;
            let units = if interleaved {
                self.mcus_x * self.mcus_y
            } else {
                let ci = scan_comps[0];
                self.comps[ci].bx * self.comps[ci].by
            };
            let mut since_restart = 0usize;
            for unit in 0..units {
                if restart_interval > 0 && unit > 0 && since_restart == restart_interval {
                    rd.restart();
                    preds.iter_mut().for_each(|p| *p = 0);
                    since_restart = 0;
                }
                since_restart += 1;
                if interleaved {
                    let (mx, my) = (unit % self.mcus_x, unit / self.mcus_x);
                    for &ci in scan_comps {
                        let (ch, cv) = (self.comps[ci].h, self.comps[ci].v);
                        let bxw = self.comps[ci].bx;
                        for by in 0..cv {
                            for bx in 0..ch {
                                let col = mx * ch + bx;
                                let row = my * cv + by;
                                let bi = row * bxw + col;
                                self.dc_block(&mut rd, comps, dc_tabs, ci, bi, ah, al, &mut preds)?;
                            }
                        }
                    }
                } else {
                    let ci = scan_comps[0];
                    self.dc_block(&mut rd, comps, dc_tabs, ci, unit, ah, al, &mut preds)?;
                }
            }
        } else {
            // AC scan — exactly one component, over its own block grid.
            let ci = scan_comps[0];
            let units = self.comps[ci].bx * self.comps[ci].by;
            let mut since_restart = 0usize;
            for unit in 0..units {
                if restart_interval > 0 && unit > 0 && since_restart == restart_interval {
                    rd.restart();
                    self.eobrun = 0;
                    since_restart = 0;
                }
                since_restart += 1;
                if ah == 0 {
                    self.ac_first(&mut rd, comps, ac_tabs, ci, unit, ss, se, al)?;
                } else {
                    self.ac_refine(&mut rd, comps, ac_tabs, ci, unit, ss, se, al)?;
                }
            }
        }
        Some(rd.seek_next_marker())
    }

    /// Decode one **arithmetic** progressive scan (SOF10) into the coefficient
    /// store, dispatching to the §G.2/§F.1.4 DC/AC first/refine procedures.
    /// Statistics reset at scan start and each restart; the MQ decoder is
    /// re-initialised at each RSTn. Returns the byte position of the next marker.
    #[allow(clippy::too_many_arguments)]
    fn decode_scan_arith(
        &mut self,
        data: &[u8],
        start: usize,
        comps: &[Component],
        arith_dc: &[ArithDcCond; 4],
        arith_ac: &[ArithAcCond; 4],
        scan_comps: &[usize],
        ss: u8,
        se: u8,
        ah: u8,
        al: u8,
        restart_interval: usize,
    ) -> Option<usize> {
        if scan_comps.is_empty() {
            return Some(start);
        }
        let mut mq = MqDecoder::new(data, start);
        // Fresh statistics for this scan.
        let mut dc_stats: Vec<ArithStats> = (0..4).map(|_| vec![0u8; 64]).collect();
        let mut ac_stats: Vec<ArithStats> = (0..4).map(|_| vec![0u8; 256]).collect();
        let mut sign_bin: ArithStats = vec![0u8; 1];
        let mut dc_ctx = vec![0usize; self.comps.len()];
        let mut preds = vec![0i32; self.comps.len()];

        if ss == 0 {
            // DC scan (may be interleaved across components, MCU order).
            let interleaved = scan_comps.len() > 1;
            let units = if interleaved {
                self.mcus_x * self.mcus_y
            } else {
                let ci = scan_comps[0];
                self.comps[ci].bx * self.comps[ci].by
            };
            let mut since_restart = 0usize;
            for unit in 0..units {
                if restart_interval > 0 && unit > 0 && since_restart == restart_interval {
                    mq = arith_restart_reset(
                        data,
                        mq.bp,
                        &mut dc_stats,
                        &mut ac_stats,
                        &mut sign_bin,
                        &mut dc_ctx,
                        &mut preds,
                    )?;
                    since_restart = 0;
                }
                since_restart += 1;
                if interleaved {
                    let (mx, my) = (unit % self.mcus_x, unit / self.mcus_x);
                    for &ci in scan_comps {
                        let (ch, cv) = (self.comps[ci].h, self.comps[ci].v);
                        let bxw = self.comps[ci].bx;
                        for by in 0..cv {
                            for bx in 0..ch {
                                let bi = (my * cv + by) * bxw + (mx * ch + bx);
                                self.dc_block_arith(
                                    &mut mq,
                                    comps,
                                    &mut dc_stats,
                                    arith_dc,
                                    &mut dc_ctx,
                                    &mut preds,
                                    ci,
                                    bi,
                                    ah,
                                    al,
                                )?;
                            }
                        }
                    }
                } else {
                    let ci = scan_comps[0];
                    self.dc_block_arith(
                        &mut mq,
                        comps,
                        &mut dc_stats,
                        arith_dc,
                        &mut dc_ctx,
                        &mut preds,
                        ci,
                        unit,
                        ah,
                        al,
                    )?;
                }
            }
        } else {
            // AC scan — exactly one component over its own block grid.
            let ci = scan_comps[0];
            let units = self.comps[ci].bx * self.comps[ci].by;
            let mut since_restart = 0usize;
            for unit in 0..units {
                if restart_interval > 0 && unit > 0 && since_restart == restart_interval {
                    mq = arith_restart_reset(
                        data,
                        mq.bp,
                        &mut dc_stats,
                        &mut ac_stats,
                        &mut sign_bin,
                        &mut dc_ctx,
                        &mut preds,
                    )?;
                    since_restart = 0;
                }
                since_restart += 1;
                if ah == 0 {
                    self.ac_first_arith(
                        &mut mq, comps, &mut ac_stats, &mut sign_bin, arith_ac, ci, unit, ss, se, al,
                    )?;
                } else {
                    self.ac_refine_arith(
                        &mut mq, comps, &mut ac_stats, &mut sign_bin, ci, unit, ss, se, al,
                    )?;
                }
            }
        }
        Some(resync_to_next_marker(data, mq.bp))
    }

    /// Arithmetic DC first-scan / refinement for one block (§G.2.1).
    #[allow(clippy::too_many_arguments)]
    fn dc_block_arith(
        &mut self,
        mq: &mut MqDecoder,
        comps: &[Component],
        dc_stats: &mut [ArithStats],
        arith_dc: &[ArithDcCond; 4],
        dc_ctx: &mut [usize],
        preds: &mut [i32],
        ci: usize,
        bi: usize,
        ah: u8,
        al: u8,
    ) -> Option<()> {
        let tbl = comps[ci].dc_tab.min(3);
        if ah == 0 {
            let diff = arith_decode_dc(mq, &mut dc_stats[tbl], arith_dc[tbl], &mut dc_ctx[ci])?;
            preds[ci] += diff;
            self.comps[ci].coeffs.get_mut(bi)?[0] = preds[ci] << al;
        } else {
            // Refinement: one bit via the fixed bin (the DC table's bin 0 acts as
            // the dedicated refinement context for this scan).
            if mq.decode(&mut dc_stats[tbl], 0) == 1 {
                self.comps[ci].coeffs.get_mut(bi)?[0] |= 1 << al;
            }
        }
        Some(())
    }

    /// Arithmetic AC first-scan for a band `[ss, se]` of one block (§G.2.2,
    /// Ah == 0).
    #[allow(clippy::too_many_arguments)]
    fn ac_first_arith(
        &mut self,
        mq: &mut MqDecoder,
        comps: &[Component],
        ac_stats: &mut [ArithStats],
        sign_bin: &mut ArithStats,
        arith_ac: &[ArithAcCond; 4],
        ci: usize,
        bi: usize,
        ss: u8,
        se: u8,
        al: u8,
    ) -> Option<()> {
        let tbl = comps[ci].ac_tab.min(3);
        let mut tmp = [0i32; 64];
        arith_decode_ac(
            mq,
            &mut ac_stats[tbl],
            sign_bin,
            arith_ac[tbl],
            &mut tmp,
            ss as usize,
            se as usize,
            al,
        )?;
        // `arith_decode_ac` wrote natural-order; the store is zig-zag.
        let block = self.comps[ci].coeffs.get_mut(bi)?;
        for k in ss as usize..=se as usize {
            block[k] = tmp[ZIGZAG[k]];
        }
        Some(())
    }

    /// Arithmetic AC refinement scan for a band `[ss, se]` of one block (§G.2.4).
    #[allow(clippy::too_many_arguments)]
    fn ac_refine_arith(
        &mut self,
        mq: &mut MqDecoder,
        comps: &[Component],
        ac_stats: &mut [ArithStats],
        sign_bin: &mut ArithStats,
        ci: usize,
        bi: usize,
        ss: u8,
        se: u8,
        al: u8,
    ) -> Option<()> {
        let tbl = comps[ci].ac_tab.min(3);
        let p1 = 1i32 << al;
        let m1 = -(1i32 << al);
        let stats = &mut ac_stats[tbl];
        let block = self.comps[ci].coeffs.get_mut(bi)?;
        let mut k = ss as usize;
        while k <= se as usize {
            let st = 3 * (k - 1);
            if mq.decode(stats, st) == 1 {
                break; // EOB
            }
            loop {
                let st_k = 3 * (k - 1);
                if block[k] != 0 {
                    // Already-nonzero: optional correction bit.
                    if mq.decode(stats, st_k + 2) == 1 && (block[k] & p1) == 0 {
                        block[k] += if block[k] >= 0 { p1 } else { m1 };
                    }
                    break;
                } else {
                    if mq.decode(stats, st_k + 1) == 1 {
                        // Newly nonzero: sign then magnitude bit at this plane.
                        block[k] = if mq.decode(sign_bin, 0) == 1 { m1 } else { p1 };
                        break;
                    }
                    k += 1;
                    if k > se as usize {
                        return Some(());
                    }
                }
            }
            k += 1;
        }
        Some(())
    }

    /// Decode/refine the DC coefficient of one block.
    #[allow(clippy::too_many_arguments)]
    fn dc_block(
        &mut self,
        rd: &mut ProgReader,
        comps: &[Component],
        dc_tabs: &[HashMap<(u8, u16), u8>],
        ci: usize,
        bi: usize,
        ah: u8,
        al: u8,
        preds: &mut [i32],
    ) -> Option<()> {
        if ah == 0 {
            let t = rd.decode_huff(&dc_tabs[comps[ci].dc_tab.min(3)])?;
            let diff = rd.receive_extend(t);
            preds[ci] += diff;
            self.comps[ci].coeffs[bi][0] = preds[ci] << al;
        } else {
            // Refinement: append one low-order bit.
            if rd.bit() == 1 {
                self.comps[ci].coeffs[bi][0] |= 1 << al;
            }
        }
        Some(())
    }

    /// First AC scan for a band `[ss, se]` of one block (Ah == 0).
    #[allow(clippy::too_many_arguments)]
    fn ac_first(
        &mut self,
        rd: &mut ProgReader,
        comps: &[Component],
        ac_tabs: &[HashMap<(u8, u16), u8>],
        ci: usize,
        bi: usize,
        ss: u8,
        se: u8,
        al: u8,
    ) -> Option<()> {
        if self.eobrun > 0 {
            self.eobrun -= 1;
            return Some(());
        }
        let tab = &ac_tabs[comps[ci].ac_tab.min(3)];
        let block = &mut self.comps[ci].coeffs[bi];
        let mut k = ss as usize;
        while k <= se as usize {
            let rs = rd.decode_huff(tab)?;
            let run = (rs >> 4) as usize;
            let size = rs & 0x0F;
            if size == 0 {
                if run < 15 {
                    // EOB run: 2^run + (run extra bits) − 1 more blocks are EOB.
                    self.eobrun = (1 << run) - 1;
                    if run > 0 {
                        self.eobrun += rd.receive(run as u8) as u32;
                    }
                    break;
                }
                k += 16; // ZRL — 16 zeros
                continue;
            }
            k += run;
            if k > se as usize {
                break;
            }
            let val = rd.receive_extend(size);
            block[k] = val << al;
            k += 1;
        }
        Some(())
    }

    /// AC refinement scan for a band `[ss, se]` of one block (Ah > 0). This is
    /// the subtle bit-plane refinement of ISO 10918-1 §G.1.2.3.
    #[allow(clippy::too_many_arguments)]
    fn ac_refine(
        &mut self,
        rd: &mut ProgReader,
        comps: &[Component],
        ac_tabs: &[HashMap<(u8, u16), u8>],
        ci: usize,
        bi: usize,
        ss: u8,
        se: u8,
        al: u8,
    ) -> Option<()> {
        let tab = &ac_tabs[comps[ci].ac_tab.min(3)];
        let bit = 1i32 << al;
        let block = &mut self.comps[ci].coeffs[bi];
        let mut k = ss as usize;
        if self.eobrun == 0 {
            while k <= se as usize {
                let rs = rd.decode_huff(tab)?;
                let mut run = (rs >> 4) as i32;
                let size = rs & 0x0F;
                let mut newval = 0i32;
                if size == 0 {
                    if run < 15 {
                        self.eobrun = (1 << run) - 1;
                        if run > 0 {
                            self.eobrun += rd.receive(run as u8) as u32;
                        }
                        break;
                    }
                    // run == 15: skip 16 zero-history coefficients (ZRL).
                } else {
                    // size must be 1 in a refinement scan; the bit gives the sign.
                    newval = if rd.bit() == 1 { bit } else { -bit };
                }
                // Advance over `run` zero-history coefficients, refining any
                // already-nonzero coefficients we pass.
                while k <= se as usize {
                    if block[k] != 0 {
                        if rd.bit() == 1 && (block[k] & bit) == 0 {
                            block[k] += if block[k] > 0 { bit } else { -bit };
                        }
                    } else {
                        if run == 0 {
                            break;
                        }
                        run -= 1;
                    }
                    k += 1;
                }
                if newval != 0 && k <= se as usize {
                    block[k] = newval;
                }
                k += 1;
            }
        }
        if self.eobrun > 0 {
            // Refine all remaining nonzero coefficients in this block, then
            // consume one unit of the EOB run.
            while k <= se as usize {
                if block[k] != 0 && rd.bit() == 1 && (block[k] & bit) == 0 {
                    block[k] += if block[k] > 0 { bit } else { -bit };
                }
                k += 1;
            }
            self.eobrun -= 1;
        }
        Some(())
    }

    /// Dequantize, inverse-DCT, upsample chroma, and colour-convert into RGBA.
    fn finish(&self, _comps: &[Component], quant: &[[u16; 64]; 4]) -> (u32, u32, Vec<u8>) {
        let (w, h) = (self.width, self.height);
        let mut planes: Vec<Vec<f32>> = self.comps.iter().map(|_| vec![0f32; w * h]).collect();
        for (ci, pc) in self.comps.iter().enumerate() {
            let q = &quant[pc.quant.min(3)];
            let sx = self.hmax / pc.h;
            let sy = self.vmax / pc.v;
            for byk in 0..pc.by {
                for bxk in 0..pc.bx {
                    let blk = &pc.coeffs[byk * pc.bx + bxk];
                    // Dequantize + de-zigzag into natural order, then IDCT.
                    let mut nat = [0f32; 64];
                    for k in 0..64 {
                        nat[ZIGZAG[k]] = blk[k] as f32 * q[ZIGZAG[k]] as f32;
                    }
                    transform_2d(&mut nat, dct_iii);
                    place_block(&mut planes[ci], &nat, bxk, byk, sx, sy, w, h);
                }
            }
        }
        (w as u32, h as u32, planes_to_rgba(&planes, w, h))
    }
}

fn be16(d: &[u8], o: usize) -> u16 {
    ((*d.get(o).unwrap_or(&0) as u16) << 8) | *d.get(o + 1).unwrap_or(&0) as u16
}

// ── Arithmetic decoding (ISO/IEC 10918-1 Annex; MQ/QM-coder = ITU-T T.81/T.82) ─
//
// JPEG arithmetic coding uses the binary MQ arithmetic coder of the T.82/JBIG
// family with the §F.1.4 DC/AC context models. The entropy stream is decoded by
// `MqDecoder` (INITDEC/DECODE/RENORMD/BYTEIN per T.81 Figures D.16–E.19) feeding
// per-component DC/AC statistics areas; the resulting coefficients flow into the
// same dequant + IDCT + upsample + colour pipeline as the Huffman paths.

/// One DAC DC conditioning entry: the `(L, U)` bounds that classify the previous
/// block's DC difference into the 5 context zones (§F.1.4.4.1.2). Default L=0,
/// U=1 (ISO 10918-1 §F.1.4.4.1.4) when no DAC marker is present.
#[derive(Clone, Copy)]
struct ArithDcCond {
    l: u8,
    u: u8,
}

impl Default for ArithDcCond {
    fn default() -> Self {
        ArithDcCond { l: 0, u: 1 }
    }
}

/// One DAC AC conditioning entry: the `Kx` threshold splitting the AC magnitude
/// context (§F.1.4.4.2). Default Kx=5 when no DAC marker is present.
#[derive(Clone, Copy)]
struct ArithAcCond {
    kx: u8,
}

impl Default for ArithAcCond {
    fn default() -> Self {
        ArithAcCond { kx: 5 }
    }
}

/// `Qe` probability-estimation state table (ISO/IEC 10918-1; identical to ITU-T
/// T.82 Table E.1 / JPEG2000 Annex C): `(Qe, NMPS, NLPS, SWITCH)` per state.
const QE: [(u32, u8, u8, u8); 47] = [
    (0x5601, 1, 1, 1),
    (0x3401, 2, 6, 0),
    (0x1801, 3, 9, 0),
    (0x0AC1, 4, 12, 0),
    (0x0521, 5, 29, 0),
    (0x0221, 38, 33, 0),
    (0x5601, 7, 6, 1),
    (0x5401, 8, 14, 0),
    (0x4801, 9, 14, 0),
    (0x3801, 10, 14, 0),
    (0x3001, 11, 17, 0),
    (0x2401, 12, 18, 0),
    (0x1C01, 13, 20, 0),
    (0x1601, 29, 21, 0),
    (0x5601, 15, 14, 1),
    (0x5401, 16, 14, 0),
    (0x5101, 17, 15, 0),
    (0x4801, 18, 16, 0),
    (0x3801, 19, 17, 0),
    (0x3401, 20, 18, 0),
    (0x3001, 21, 19, 0),
    (0x2801, 22, 19, 0),
    (0x2401, 23, 20, 0),
    (0x2201, 24, 21, 0),
    (0x1C01, 25, 22, 0),
    (0x1801, 26, 23, 0),
    (0x1601, 27, 24, 0),
    (0x1401, 28, 25, 0),
    (0x1201, 29, 26, 0),
    (0x1101, 30, 27, 0),
    (0x0AC1, 31, 28, 0),
    (0x09C1, 32, 29, 0),
    (0x08A1, 33, 30, 0),
    (0x0521, 34, 31, 0),
    (0x0441, 35, 32, 0),
    (0x02A1, 36, 33, 0),
    (0x0221, 37, 34, 0),
    (0x0141, 38, 35, 0),
    (0x0111, 39, 36, 0),
    (0x0085, 40, 37, 0),
    (0x0049, 41, 38, 0),
    (0x0025, 42, 39, 0),
    (0x0015, 43, 40, 0),
    (0x0009, 44, 41, 0),
    (0x0005, 45, 42, 0),
    (0x0001, 45, 43, 0),
    (0x5601, 46, 46, 0),
];

/// A statistics area: one `(index, mps)` per context bin, packed as a single
/// `u8` (low 7 bits = `Qe` state index 0..=46, bit 7 = MPS). All bins start at
/// state 0, MPS 0 (ISO 10918-1 §F.1.4 initial conditioning).
type ArithStats = Vec<u8>;

#[inline]
fn stat_index(s: u8) -> usize {
    (s & 0x7F) as usize
}
#[inline]
fn stat_mps(s: u8) -> u8 {
    s >> 7
}
#[inline]
fn pack_stat(index: u8, mps: u8) -> u8 {
    (index & 0x7F) | (mps << 7)
}

/// MQ arithmetic decoder over the entropy-coded segment (T.81 software
/// conventions: the code register `c` holds the comparison value in its high
/// bits, `a` is the 16-bit sub-interval width).
struct MqDecoder<'a> {
    data: &'a [u8],
    /// Index of the byte most recently loaded into `c` (BYTEIN tests `data[bp]`
    /// to detect `0xFF` stuffing / markers).
    bp: usize,
    a: u32,
    c: u32,
    ct: i32,
}

impl<'a> MqDecoder<'a> {
    /// INITDEC (T.81 Figure E.20): prime `c` from the first two bytes.
    fn new(data: &'a [u8], start: usize) -> MqDecoder<'a> {
        let mut d = MqDecoder {
            data,
            bp: start,
            a: 0,
            c: 0,
            ct: 0,
        };
        let b0 = d.byte_at(d.bp);
        d.c = (b0 as u32) << 16;
        d.byte_in();
        d.c <<= 7;
        d.ct -= 7;
        d.a = 0x8000;
        d
    }

    #[inline]
    fn byte_at(&self, i: usize) -> u8 {
        *self.data.get(i).unwrap_or(&0xFF)
    }

    /// BYTEIN (T.81 Figure E.19): feed the next byte into `c`, honouring `0xFF`
    /// stuffing and stopping (supplying `0xFF00`) at a real marker.
    fn byte_in(&mut self) {
        if self.byte_at(self.bp) == 0xFF {
            let b1 = self.byte_at(self.bp + 1);
            if b1 > 0x8F {
                // Marker (≥ 0xFF90 — note 0xFF00 stuffing has b1 == 0x00): do
                // not advance; supply 1-bits.
                self.c += 0xFF00;
                self.ct = 8;
            } else {
                self.bp += 1;
                self.c += (b1 as u32) << 9;
                self.ct = 7;
            }
        } else {
            self.bp += 1;
            self.c += (self.byte_at(self.bp) as u32) << 8;
            self.ct = 8;
        }
    }

    /// RENORMD (T.81 Figure E.18): shift `a`/`c` left until `a >= 0x8000`.
    #[inline]
    fn renorm(&mut self) {
        loop {
            if self.ct == 0 {
                self.byte_in();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }

    /// DECODE (T.81 Figure D.16, QM-coder convention with the LPS sub-interval
    /// at the bottom): decode one binary decision against statistics bin `st`.
    fn decode(&mut self, stats: &mut ArithStats, st: usize) -> u32 {
        // Bounds-safe against corrupt streams that drive `st` out of range
        // (`panic = "abort"` forbids a panic on malformed input).
        let Some(slot) = stats.get_mut(st) else {
            return 0;
        };
        let sv = *slot;
        let index = stat_index(sv);
        let mps = stat_mps(sv);
        let (qe, nmps, nlps, switch) = QE[index];
        self.a = self.a.wrapping_sub(qe);
        let d;
        if (self.c >> 16) < qe {
            // LPS sub-interval (lower).  LPS_EXCHANGE + RENORMD (Figure D.19).
            if self.a < qe {
                d = mps;
                *slot = pack_stat(nmps, mps);
            } else {
                d = 1 - mps;
                let new_mps = if switch == 1 { 1 - mps } else { mps };
                *slot = pack_stat(nlps, new_mps);
            }
            self.a = qe;
            self.renorm();
        } else {
            self.c -= qe << 16;
            if self.a & 0x8000 == 0 {
                // MPS_EXCHANGE + RENORMD (Figure D.18).
                if self.a < qe {
                    d = 1 - mps;
                    let new_mps = if switch == 1 { 1 - mps } else { mps };
                    *slot = pack_stat(nlps, new_mps);
                } else {
                    d = mps;
                    *slot = pack_stat(nmps, mps);
                }
                self.renorm();
            } else {
                d = mps;
            }
        }
        d as u32
    }
}

/// Decode one DC coefficient difference for component context `dc_ctx`
/// (§F.1.4.4.1, Figures F.19–F.24). `stats` is this DC table's statistics area
/// (≥ 49 bins). Returns the signed diff and updates `*dc_ctx` (the conditioning
/// category carried to the next block of this component).
fn arith_decode_dc(
    mq: &mut MqDecoder,
    stats: &mut ArithStats,
    cond: ArithDcCond,
    dc_ctx: &mut usize,
) -> Option<i32> {
    let base = *dc_ctx; // 0, 4, 8, 12, or 16
    if mq.decode(stats, base) == 0 {
        *dc_ctx = 0;
        return Some(0);
    }
    // Non-zero: sign, then magnitude category, then magnitude low bits.
    let sign = mq.decode(stats, base + 1); // 0 = positive, 1 = negative
    // SP/SN entry bin: base+2 (positive) or base+3 (negative).
    let mut st = base + 2 + sign as usize;
    let mut m: i32 = mq.decode(stats, st) as i32;
    if m != 0 {
        // Magnitude category ladder uses the table-wide X bins at offset 20.
        st = 20;
        m = 1;
        while mq.decode(stats, st) == 1 {
            m <<= 1;
            if m == 0x8000 {
                return None; // overflow guard (corrupt stream)
            }
            st += 1;
        }
    }
    // Establish the conditioning category for the NEXT block (§F.1.4.4.1.2).
    let lower = (1i32 << cond.l) >> 1;
    let upper = (1i32 << cond.u) >> 1;
    *dc_ctx = if m < lower {
        0
    } else if m > upper {
        12 + (sign as usize) * 4
    } else {
        4 + (sign as usize) * 4
    };
    // Decode the remaining magnitude bits (MSB already implied by `m`).
    let mut v = m;
    let mut bit = m >> 1;
    while bit != 0 {
        if mq.decode(stats, st) == 1 {
            v |= bit;
        }
        bit >>= 1;
    }
    v += 1;
    Some(if sign == 1 { -v } else { v })
}

/// Decode the AC coefficients of one block into `block` (natural-order, scaled
/// by `1 << al`), §F.1.4.4.2 Figures F.15–F.18. `stats` is this AC table's
/// statistics area (≥ 245 bins); `sign_bin` is the shared fixed-probability sign
/// bin. Coefficients k = ss..=se in zig-zag are written.
#[allow(clippy::too_many_arguments)]
fn arith_decode_ac(
    mq: &mut MqDecoder,
    stats: &mut ArithStats,
    sign_bin: &mut ArithStats,
    cond: ArithAcCond,
    block: &mut [i32; 64],
    ss: usize,
    se: usize,
    al: u8,
) -> Option<()> {
    let mut k = ss;
    while k <= se {
        let base = 3 * (k - 1);
        if mq.decode(stats, base) == 1 {
            break; // EOB
        }
        // Advance over the zero run until a non-zero coefficient at position k.
        loop {
            if mq.decode(stats, base_of(k) + 1) == 1 {
                break;
            }
            k += 1;
            if k > se {
                return Some(()); // ran past the band — done
            }
        }
        let bk = base_of(k);
        // Sign uses the fixed (non-adapting per spec, but harmless if it adapts)
        // bin shared across the scan.
        let sign = mq.decode(sign_bin, 0);
        // Magnitude category: first bit at bk+2, then the size ladder.
        let mut st = bk + 2;
        let mut m: i32 = mq.decode(stats, st) as i32;
        // `&&` short-circuits: the second magnitude decision is only consumed
        // (and only mutates the MQ state) when the first bit was non-zero.
        if m != 0 && mq.decode(stats, st) == 1 {
            m = 2;
            // Extension bins depend on whether k ≤ Kx.
            st = if k <= cond.kx as usize { 189 } else { 217 };
            while mq.decode(stats, st) == 1 {
                m <<= 1;
                if m == 0x8000 {
                    return None; // overflow guard
                }
                st += 1;
            }
        }
        let mut v = m;
        let mut bit = m >> 1;
        while bit != 0 {
            if mq.decode(stats, st) == 1 {
                v |= bit;
            }
            bit >>= 1;
        }
        v += 1;
        let val = if sign == 1 { -v } else { v };
        block[ZIGZAG[k]] = val << al;
        k += 1;
    }
    Some(())
}

#[inline]
fn base_of(k: usize) -> usize {
    3 * (k - 1)
}

/// Arithmetic-sequential (SOF9) scan: mirrors [`decode_scan`] (MCU interleave,
/// block placement, colour-convert) but decodes each block with the MQ coder and
/// the §F.1.4 DC/AC context models. Honours restart intervals (statistics and DC
/// predictors reset, MQ decoder re-initialised at each RSTn).
#[allow(clippy::too_many_arguments)]
fn decode_scan_arith(
    data: &[u8],
    start: usize,
    width: u32,
    height: u32,
    comps: &mut [Component],
    quant: &[[u16; 64]; 4],
    arith_dc: &[ArithDcCond; 4],
    arith_ac: &[ArithAcCond; 4],
    restart_interval: usize,
) -> Option<(u32, u32, Vec<u8>)> {
    if width == 0 || height == 0 || comps.is_empty() {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    let hmax = comps.iter().map(|c| c.h).max()?.max(1) as usize;
    let vmax = comps.iter().map(|c| c.v).max()?.max(1) as usize;
    let mcus_x = w.div_ceil(8 * hmax);
    let mcus_y = h.div_ceil(8 * vmax);

    let mut planes: Vec<Vec<f32>> = comps.iter().map(|_| vec![0f32; w * h]).collect();

    // Statistics areas (one DC + one AC per table id) and the shared sign bin.
    let mut dc_stats: Vec<ArithStats> = (0..4).map(|_| vec![0u8; 64]).collect();
    let mut ac_stats: Vec<ArithStats> = (0..4).map(|_| vec![0u8; 256]).collect();
    let mut sign_bin: ArithStats = vec![0u8; 1];
    let mut dc_ctx = vec![0usize; comps.len()];

    let mut mq = MqDecoder::new(data, start);
    let mut bp_marker = start; // tracked so we can resume the marker loop after
    let mut since_restart = 0usize;
    let total = mcus_x * mcus_y;
    for mi in 0..total {
        if restart_interval > 0 && mi > 0 && since_restart == restart_interval {
            // Resync at the RSTn marker, reset entropy state.
            let next = resync_restart(data, mq.bp)?;
            mq = MqDecoder::new(data, next);
            for s in dc_stats.iter_mut() {
                s.iter_mut().for_each(|b| *b = 0);
            }
            for s in ac_stats.iter_mut() {
                s.iter_mut().for_each(|b| *b = 0);
            }
            sign_bin[0] = 0;
            comps.iter_mut().for_each(|c| c.pred = 0);
            dc_ctx.iter_mut().for_each(|x| *x = 0);
            since_restart = 0;
        }
        since_restart += 1;
        let (mx, my) = (mi % mcus_x, mi / mcus_x);
        for (ci, c) in comps.iter_mut().enumerate() {
            let (ch, cv) = (c.h as usize, c.v as usize);
            let sx = hmax / ch;
            let sy = vmax / cv;
            let dctab = c.dc_tab.min(3);
            let actab = c.ac_tab.min(3);
            for by in 0..cv {
                for bx in 0..ch {
                    let mut coeffs = [0i32; 64];
                    let diff = arith_decode_dc(
                        &mut mq,
                        &mut dc_stats[dctab],
                        arith_dc[dctab],
                        &mut dc_ctx[ci],
                    )?;
                    c.pred += diff;
                    coeffs[0] = c.pred;
                    arith_decode_ac(
                        &mut mq,
                        &mut ac_stats[actab],
                        &mut sign_bin,
                        arith_ac[actab],
                        &mut coeffs,
                        1,
                        63,
                        0,
                    )?;
                    // Dequantize (natural order) + IDCT → spatial block.
                    let q = &quant[c.quant.min(3)];
                    let mut blk = [0f32; 64];
                    for i in 0..64 {
                        blk[i] = coeffs[i] as f32 * q[i] as f32;
                    }
                    transform_2d(&mut blk, dct_iii);
                    place_block(&mut planes[ci], &blk, mx * ch + bx, my * cv + by, sx, sy, w, h);
                }
            }
        }
        bp_marker = mq.bp;
    }
    let _ = bp_marker;
    Some((width, height, planes_to_rgba(&planes, w, h)))
}

/// Skip to and past the next restart marker (`FF D0..D7`) starting near `from`,
/// returning the byte index just after it (for a fresh `MqDecoder`). `None` if
/// no restart marker is found.
fn resync_restart(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < data.len() {
        if data[i] == 0xFF {
            let m = data[i + 1];
            if (0xD0..=0xD7).contains(&m) {
                return Some(i + 2);
            }
            if m == 0x00 || m == 0xFF {
                i += 1; // stuffed byte or fill — keep scanning
                continue;
            }
            // Some other marker before a restart: give up gracefully.
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Advance from `from` to the next genuine marker (skipping stuffed `FF00`,
/// fill `FFFF`, and restart `FF D0..D7` bytes that are part of the scan),
/// returning its byte index so the caller's marker loop can resume.
fn resync_to_next_marker(data: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 1 < data.len() {
        if data[i] == 0xFF {
            let m = data[i + 1];
            if m == 0x00 || m == 0xFF || (0xD0..=0xD7).contains(&m) {
                i += 1; // stuffed / fill / restart — part of the entropy scan
                continue;
            }
            return i; // a real marker
        }
        i += 1;
    }
    i
}

/// At a restart boundary in an arithmetic scan: resync past the RSTn marker,
/// zero all statistics + DC predictors, and return a fresh `MqDecoder` bound to
/// the same `data` (kept as a free fn so its lifetime is tied to `data`, not to
/// any enclosing `&mut MqDecoder`). `None` if no restart marker is found.
#[allow(clippy::too_many_arguments)]
fn arith_restart_reset<'a>(
    data: &'a [u8],
    from: usize,
    dc_stats: &mut [ArithStats],
    ac_stats: &mut [ArithStats],
    sign_bin: &mut ArithStats,
    dc_ctx: &mut [usize],
    preds: &mut [i32],
) -> Option<MqDecoder<'a>> {
    let next = resync_restart(data, from)?;
    for s in dc_stats.iter_mut() {
        s.iter_mut().for_each(|b| *b = 0);
    }
    for s in ac_stats.iter_mut() {
        s.iter_mut().for_each(|b| *b = 0);
    }
    sign_bin.iter_mut().for_each(|b| *b = 0);
    dc_ctx.iter_mut().for_each(|x| *x = 0);
    preds.iter_mut().for_each(|p| *p = 0);
    Some(MqDecoder::new(data, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let p = ((y * w + x) * 4) as usize;
                v[p] = (x * 255 / w.max(1)) as u8;
                v[p + 1] = (y * 255 / h.max(1)) as u8;
                v[p + 2] = 128;
                v[p + 3] = 255;
            }
        }
        v
    }

    #[test]
    fn bitwriter_flush_pads_final_byte_without_corrupting_code() {
        // 7 written zero-bits + flush ⇒ 7 zeros and ONE 1-bit pad = 0b0000_0001.
        // The old `put(0x7F, 1)` OR-ed seven 1-bits over the written zeros and
        // produced 0x7F — corrupting the final code. This is the regression.
        let mut w = BitWriter::new();
        w.put(0b000_0000, 7);
        w.flush();
        assert_eq!(w.out, vec![0b0000_0001]);

        // 5 written bits 0b10101 + flush (3 pad bits) ⇒ 0b10101_111 = 0xAF.
        // The buggy version yielded 0xFF (+ a stuffed 0x00).
        let mut w = BitWriter::new();
        w.put(0b1_0101, 5);
        w.flush();
        assert_eq!(w.out, vec![0xAF]);

        // A final byte that legitimately becomes 0xFF must still be 0x00-stuffed.
        let mut w = BitWriter::new();
        w.put(0b111, 3);
        w.flush();
        assert_eq!(w.out, vec![0xFF, 0x00]);
    }

    #[test]
    fn round_trips_a_gradient_within_tolerance() {
        let (w, h) = (24u32, 16u32);
        let src = gradient(w, h);
        let jpg = encode_jpeg(w, h, &src, 92);
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9], "EOI");
        let (dw, dh, dec) = decode_jpeg(&jpg).expect("decodes");
        assert_eq!((dw, dh), (w, h));
        // Baseline JPEG is lossy; at q92 4:4:4 the error stays small.
        let mut max_err = 0i32;
        let mut sum = 0i64;
        for i in 0..(w * h) as usize {
            for c in 0..3 {
                let d = (src[i * 4 + c] as i32 - dec[i * 4 + c] as i32).abs();
                max_err = max_err.max(d);
                sum += d as i64;
            }
        }
        let mean = sum as f64 / (w * h * 3) as f64;
        assert!(mean < 4.0, "mean abs error {mean} too high");
        assert!(max_err < 24, "max abs error {max_err} too high");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(encode_jpeg(0, 0, &[], 90).is_empty());
        assert!(encode_jpeg(2, 2, &[0; 3], 90).is_empty());
        assert!(decode_jpeg(&[0, 1, 2]).is_none());
    }

    #[test]
    fn solid_colour_survives_round_trip() {
        // A flat block should come back essentially unchanged.
        let src: Vec<u8> = [200u8, 100, 50, 255].repeat(8 * 8);
        let (_, _, dec) = decode_jpeg(&encode_jpeg(8, 8, &src, 90)).unwrap();
        for px in dec.chunks_exact(4) {
            assert!((px[0] as i32 - 200).abs() <= 6, "R {}", px[0]);
            assert!((px[1] as i32 - 100).abs() <= 6, "G {}", px[1]);
            assert!((px[2] as i32 - 50).abs() <= 6, "B {}", px[2]);
        }
    }

    /// Hand-assemble a minimal **progressive** (SOF2) grayscale JPEG of a single
    /// 8×8 block of constant luma `gray`, in two scans: a DC scan (Ss=0,Se=0)
    /// then an AC EOB-run scan (Ss=1,Se=63). Quant table is all-ones (lossless
    /// DC) so the decode is exact bar IDCT float rounding. Exercises the
    /// progressive DC-scan and AC-first-scan code paths.
    fn progressive_gray_8x8(gray: u8) -> Vec<u8> {
        // The decoder dequantizes (×1) then runs `dct_iii`; pick the DC
        // coefficient so a constant block round-trips. For a constant input `c`,
        // the forward DCT-II's DC term is `8 * c * alpha(0)^2 = c` is NOT exact,
        // so compute it via the real forward transform to stay consistent.
        let c = gray as f32 - 128.0;
        let mut blk = [c; 64];
        transform_2d(&mut blk, dct_ii);
        let dc = blk[0].round() as i32; // AC terms of a constant block are ~0.

        let dc_tab = build_codes(&DC_LUMA_BITS, &DC_LUMA_VALS);
        let ac_tab = build_codes(&AC_LUMA_BITS, &AC_LUMA_VALS);
        // Pad the final partial byte with exactly `8-nbits` one-bits, masked to
        // that width (the shared `BitWriter::flush` pads with `0x7F`, whose high
        // bits would bleed into the preceding code field for a sub-7-bit pad).
        let pad = |w: &mut BitWriter| {
            let rem = w.nbits % 8;
            if rem != 0 {
                let n = 8 - rem;
                w.put(((1u32 << n) - 1) as u16, n as u8);
            }
        };

        // DC scan entropy: one DC diff (predictor starts at 0).
        let mut dcw = BitWriter::new();
        let (cat, bits) = magnitude(dc);
        let (code, len) = dc_tab[&cat];
        dcw.put(code, len);
        if cat > 0 {
            dcw.put(bits, cat);
        }
        pad(&mut dcw);

        // AC scan entropy: a single EOB (symbol 0x00) — the one block is all-zero
        // AC, EOB run of 1.
        let mut acw = BitWriter::new();
        let (code, len) = ac_tab[&0x00];
        acw.put(code, len);
        pad(&mut acw);

        let mut out: Vec<u8> = vec![0xFF, 0xD8]; // SOI
                                                 // DQT (all ones).
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        out.extend_from_slice(&[1u8; 64]);
        // SOF2 — progressive, 1 component, 1×1 sampling.
        out.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
        out.extend_from_slice(&[0x01, 0x11, 0x00]);
        // DHT — DC luma (class 0, id 0) and AC luma (class 1, id 0).
        for (class_id, bits, vals) in [
            (0x00u8, &DC_LUMA_BITS[..], &DC_LUMA_VALS[..]),
            (0x10, &AC_LUMA_BITS[..], &AC_LUMA_VALS[..]),
        ] {
            let len = 19 + vals.len();
            out.extend_from_slice(&[0xFF, 0xC4, (len >> 8) as u8, (len & 0xFF) as u8, class_id]);
            out.extend_from_slice(bits);
            out.extend_from_slice(vals);
        }
        // SOS #1 — DC scan: 1 comp, Ss=0 Se=0 Ah=0 Al=0.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&dcw.out);
        // SOS #2 — AC scan: 1 comp, Ss=1 Se=63 Ah=0 Al=0, AC table id 0.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x01, 0x3F, 0x00]);
        out.extend_from_slice(&acw.out);
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        out
    }

    #[test]
    fn decodes_progressive_grayscale() {
        // A two-scan progressive JPEG of solid gray must decode to that gray.
        let jpg = progressive_gray_8x8(160);
        let (w, h, dec) = decode_jpeg(&jpg).expect("progressive decodes");
        assert_eq!((w, h), (8, 8));
        for px in dec.chunks_exact(4) {
            assert!((px[0] as i32 - 160).abs() <= 3, "R {}", px[0]);
            assert!((px[1] as i32 - 160).abs() <= 3, "G {}", px[1]);
            assert!((px[2] as i32 - 160).abs() <= 3, "B {}", px[2]);
            assert_eq!(px[3], 255);
        }
        // A second, darker value to ensure the DC scan carries real data.
        let (_, _, dec2) = decode_jpeg(&progressive_gray_8x8(64)).expect("decodes");
        assert!((dec2[0] as i32 - 64).abs() <= 3, "dark gray {}", dec2[0]);
    }

    #[test]
    fn unsupported_sof_is_rejected_gracefully() {
        // Lossless SOF3 (spatial predictor, not DCT) remains unsupported and
        // must return None (not panic), so the caller skips the single image
        // rather than blanking the page. SOF9/SOF10 arithmetic ARE now
        // supported (covered by the round-trip tests below), so this targets a
        // mode that is still unsupported.
        let mut data: Vec<u8> = vec![0xFF, 0xD8];
        data.extend_from_slice(&[0xFF, 0xC3, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
        data.extend_from_slice(&[0x01, 0x11, 0x00]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        assert!(decode_jpeg(&data).is_none());

        // A truncated / malformed arithmetic header must also fail gracefully.
        let mut bad: Vec<u8> = vec![0xFF, 0xD8];
        bad.extend_from_slice(&[0xFF, 0xC9, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
        // Frame claims 1 component but the bytes are cut off before SOS/EOI.
        assert!(decode_jpeg(&bad).is_none());
    }

    // ── Arithmetic (SOF9/SOF10) round-trip via an in-test MQ encoder ──────────
    //
    // No JPEG-arithmetic fixture is available, so we generate one: a minimal
    // MQ **encoder** (the exact structural inverse of `MqDecoder`, sharing the
    // `QE` table — T.81 QM-coder ENCODE/RENORME/BYTEOUT/FLUSH) plus the DC/AC
    // arithmetic *encoders* mirroring `arith_decode_dc`/`arith_decode_ac`. If
    // the decoder round-trips this encoder for several inputs, both are
    // correct by symmetry.

    /// Test-only MQ encoder — the exact inverse of [`MqDecoder`], producing the
    /// canonical MQ byte stream (BYTEOUT/RENORME/FLUSH per the ISO/IEC 10918-1
    /// Annex / JPEG2000 MQ-coder, OpenJPEG software conventions). `out[0]` is a
    /// sentinel byte (stripped by `finish`) so the first BYTEOUT has a "previous
    /// byte" to test for carry/stuffing; `bp` indexes the current output byte.
    struct MqEncoder {
        out: Vec<u8>,
        a: u32,
        c: u32,
        ct: i32,
        bp: usize,
    }

    impl MqEncoder {
        fn new() -> MqEncoder {
            // INITENC: A=0x8000, C=0, CT=12, sentinel byte at index 0.
            MqEncoder {
                out: vec![0u8],
                a: 0x8000,
                c: 0,
                ct: 12,
                bp: 0,
            }
        }

        /// BYTEOUT: emit one byte (`c >> 19`, or `c >> 20` + 7-bit stuff after a
        /// `0xFF`), propagating any carry into the previous byte.
        fn byte_out(&mut self) {
            if self.out[self.bp] == 0xFF {
                self.out.push((self.c >> 20) as u8);
                self.bp = self.out.len() - 1;
                self.c &= 0xFFFFF;
                self.ct = 7;
            } else if self.c & 0x8000000 == 0 {
                self.out.push((self.c >> 19) as u8);
                self.bp = self.out.len() - 1;
                self.c &= 0x7FFFF;
                self.ct = 8;
            } else {
                self.out[self.bp] += 1; // carry into the previous byte
                if self.out[self.bp] == 0xFF {
                    self.c &= 0x7FFFFFF;
                    self.out.push((self.c >> 20) as u8);
                    self.bp = self.out.len() - 1;
                    self.c &= 0xFFFFF;
                    self.ct = 7;
                } else {
                    self.out.push((self.c >> 19) as u8);
                    self.bp = self.out.len() - 1;
                    self.c &= 0x7FFFF;
                    self.ct = 8;
                }
            }
        }

        /// RENORME.
        fn renorm(&mut self) {
            loop {
                if self.ct == 0 {
                    self.byte_out();
                }
                self.a <<= 1;
                self.c <<= 1;
                self.ct -= 1;
                if self.a & 0x8000 != 0 {
                    break;
                }
            }
        }

        /// ENCODE(d) — the exact inverse of `MqDecoder::decode`, derived from its
        /// LPS/MPS exchange branches (LPS sub-interval at the bottom).
        fn encode(&mut self, stats: &mut ArithStats, st: usize, d: u32) {
            let sv = stats[st];
            let index = stat_index(sv);
            let mps = stat_mps(sv);
            let (qe, nmps, nlps, switch) = QE[index];
            self.a = self.a.wrapping_sub(qe);
            if d == mps as u32 {
                if self.a & 0x8000 != 0 {
                    self.c = self.c.wrapping_add(qe); // no renorm, upper interval
                    return;
                }
                if self.a >= qe {
                    self.c = self.c.wrapping_add(qe); // upper sub-interval
                } else {
                    self.a = qe; // conditional exchange → lower sub-interval
                }
                stats[st] = pack_stat(nmps, mps);
                self.renorm();
            } else {
                if self.a & 0x8000 != 0 {
                    self.a = qe; // lower sub-interval (A' ≥ 0x8000 ≥ Qe)
                } else if self.a >= qe {
                    self.a = qe; // lower sub-interval
                } else {
                    self.c = self.c.wrapping_add(qe); // conditional exchange → upper
                }
                let new_mps = if switch == 1 { 1 - mps } else { mps };
                stats[st] = pack_stat(nlps, new_mps);
                self.renorm();
            }
        }

        /// FLUSH: set the remaining low bits and emit two final bytes; strip the
        /// sentinel.
        fn finish(mut self) -> Vec<u8> {
            let tempc = self.c.wrapping_add(self.a);
            self.c |= 0xFFFF;
            if self.c >= tempc {
                self.c -= 0x8000;
            }
            self.c <<= self.ct;
            self.byte_out();
            self.c <<= self.ct;
            self.byte_out();
            self.out.remove(0); // drop the sentinel
            self.out
        }
    }

    /// Encode a DC difference, the exact mirror of [`arith_decode_dc`]. The JPEG
    /// arithmetic magnitude is binarised on `Sz = |diff| - 1`: the size category
    /// `cat` is the bit-length of `Sz`, `m = 1 << (cat-1)` is its MSB weight
    /// (`0` for |diff|==1), and the `cat-1` low bits of `Sz` follow MSB-first.
    fn arith_encode_dc(
        enc: &mut MqEncoder,
        stats: &mut ArithStats,
        cond: ArithDcCond,
        dc_ctx: &mut usize,
        diff: i32,
    ) {
        let base = *dc_ctx;
        if diff == 0 {
            enc.encode(stats, base, 0);
            *dc_ctx = 0;
            return;
        }
        enc.encode(stats, base, 1);
        let sign = if diff < 0 { 1u32 } else { 0 };
        enc.encode(stats, base + 1, sign);
        let mag = diff.unsigned_abs() as i32;
        let sz = mag - 1;
        let mut st = base + 2 + sign as usize;
        let (m, cat) = if sz == 0 {
            enc.encode(stats, st, 0); // |diff| == 1
            (0i32, 0u32)
        } else {
            enc.encode(stats, st, 1);
            let cat = 32 - (sz as u32).leading_zeros(); // ≥ 1
            // Size ladder at offset 20: (cat-1) ones then a zero brings the
            // decoder's `m` (starting at 1) to 1 << (cat-1).
            st = 20;
            for _ in 0..(cat - 1) {
                enc.encode(stats, st, 1);
                st += 1;
            }
            enc.encode(stats, st, 0);
            (1i32 << (cat - 1), cat)
        };
        // Next-block conditioning category (decoder classifies on `m`).
        let lower = (1i32 << cond.l) >> 1;
        let upper = (1i32 << cond.u) >> 1;
        *dc_ctx = if m < lower {
            0
        } else if m > upper {
            12 + (sign as usize) * 4
        } else {
            4 + (sign as usize) * 4
        };
        // Low magnitude bits of Sz below its MSB, MSB-first (re-using `st`).
        if cat >= 2 {
            let mut bit = 1i32 << (cat - 2);
            while bit != 0 {
                enc.encode(stats, st, ((sz & bit) != 0) as u32);
                bit >>= 1;
            }
        }
    }

    /// Encode one block's AC band, mirror of [`arith_decode_ac`]. `coeffs` is in
    /// natural order. EOB after the last nonzero in `[ss, se]`.
    #[allow(clippy::too_many_arguments)]
    fn arith_encode_ac(
        enc: &mut MqEncoder,
        stats: &mut ArithStats,
        sign_bin: &mut ArithStats,
        cond: ArithAcCond,
        coeffs: &[i32; 64],
        ss: usize,
        se: usize,
    ) {
        // Last nonzero zig-zag index in the band.
        let mut last = ss.saturating_sub(1);
        for k in ss..=se {
            if coeffs[ZIGZAG[k]] != 0 {
                last = k;
            }
        }
        let mut k = ss;
        while k <= se {
            if k > last {
                enc.encode(stats, base_of(k), 1); // EOB
                return;
            }
            enc.encode(stats, base_of(k), 0); // not EOB
            // Advance over zeros: emit run-decision 0 for each zero, 1 at nonzero.
            while coeffs[ZIGZAG[k]] == 0 {
                enc.encode(stats, base_of(k) + 1, 0);
                k += 1;
            }
            enc.encode(stats, base_of(k) + 1, 1);
            let val = coeffs[ZIGZAG[k]];
            let sign = if val < 0 { 1u32 } else { 0 };
            enc.encode(sign_bin, 0, sign);
            let mag = val.unsigned_abs() as i32;
            let sz = mag - 1;
            let st0 = base_of(k) + 2;
            // [C] first magnitude decision at st0; magnitude binarised on `Sz`.
            let (st, cat) = if sz == 0 {
                enc.encode(stats, st0, 0); // |coef| == 1
                (st0, 0u32)
            } else {
                enc.encode(stats, st0, 1);
                let cat = 32 - (sz as u32).leading_zeros(); // ≥ 1
                if cat == 1 {
                    enc.encode(stats, st0, 0); // |coef| == 2, no extension ladder
                    (st0, 1u32)
                } else {
                    enc.encode(stats, st0, 1); // second decision selects the ladder
                    let mut st = if k <= cond.kx as usize { 189 } else { 217 };
                    for _ in 0..(cat - 2) {
                        enc.encode(stats, st, 1);
                        st += 1;
                    }
                    enc.encode(stats, st, 0);
                    (st, cat)
                }
            };
            // Low magnitude bits of Sz below its MSB, MSB-first.
            if cat >= 2 {
                let mut bit = 1i32 << (cat - 2);
                while bit != 0 {
                    enc.encode(stats, st, ((sz & bit) != 0) as u32);
                    bit >>= 1;
                }
            }
            k += 1;
        }
    }

    /// Assemble a one-block (8×8) grayscale **SOF9** (sequential arithmetic)
    /// JPEG from the natural-order quantized coefficients `coeffs` (quant table
    /// is all-ones, so they are also the dequantized coefficients fed to the
    /// IDCT). Uses default conditioning (no DAC marker).
    fn arith_seq_gray_8x8(coeffs: &[i32; 64]) -> Vec<u8> {
        let mut enc = MqEncoder::new();
        let mut dc_stats: ArithStats = vec![0u8; 64];
        let mut ac_stats: ArithStats = vec![0u8; 256];
        let mut sign_bin: ArithStats = vec![0u8; 1];
        let mut dc_ctx = 0usize;
        arith_encode_dc(
            &mut enc,
            &mut dc_stats,
            ArithDcCond::default(),
            &mut dc_ctx,
            coeffs[0],
        );
        arith_encode_ac(
            &mut enc,
            &mut ac_stats,
            &mut sign_bin,
            ArithAcCond::default(),
            coeffs,
            1,
            63,
        );
        let entropy = enc.finish();

        let mut out: Vec<u8> = vec![0xFF, 0xD8]; // SOI
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]); // DQT, all ones
        out.extend_from_slice(&[1u8; 64]);
        // SOF9 — extended sequential arithmetic, 1 component, 1×1 sampling.
        out.extend_from_slice(&[0xFF, 0xC9, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
        out.extend_from_slice(&[0x01, 0x11, 0x00]);
        // SOS — 1 comp, DC/AC table ids 0, Ss=0 Se=63 Ah=Al=0.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        out.extend_from_slice(&entropy);
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        out
    }

    /// Natural-order quantized coefficients of a constant `gray` 8×8 block
    /// (all-ones quant), via the real forward DCT so the round-trip is exact bar
    /// IDCT float rounding.
    fn const_block_coeffs(gray: u8) -> [i32; 64] {
        let c = gray as f32 - 128.0;
        let mut blk = [c; 64];
        transform_2d(&mut blk, dct_ii);
        let mut q = [0i32; 64];
        for i in 0..64 {
            q[i] = blk[i].round() as i32;
        }
        q
    }

    #[test]
    fn decodes_arithmetic_sequential_solid_gray() {
        // Solid gray exercises the MQ decoder, the DC context model, and the
        // AC EOB path (the whole band is zero AC ⇒ immediate EOB at k=1).
        for &gray in &[160u8, 64, 200, 16] {
            let coeffs = const_block_coeffs(gray);
            let jpg = arith_seq_gray_8x8(&coeffs);
            let (w, h, dec) = decode_jpeg(&jpg).expect("SOF9 solid gray decodes");
            assert_eq!((w, h), (8, 8));
            for px in dec.chunks_exact(4) {
                assert!(
                    (px[0] as i32 - gray as i32).abs() <= 3,
                    "gray {gray}: got {}",
                    px[0]
                );
                assert_eq!(px[3], 255);
            }
        }
    }

    #[test]
    fn decodes_arithmetic_sequential_with_ac() {
        // A non-trivial block with several nonzero AC coefficients exercises the
        // AC run/magnitude binarization (zero runs, magnitude categories, signs).
        // Build a horizontal-gradient-like block in the spatial domain, forward
        // DCT + quantize (all-ones), then round-trip.
        let mut spatial = [0f32; 64];
        for r in 0..8 {
            for col in 0..8 {
                // A ramp plus a little vertical variation → many AC terms.
                spatial[r * 8 + col] = (col as f32 * 14.0 - 50.0) + (r as f32 * 4.0);
            }
        }
        transform_2d(&mut spatial, dct_ii);
        let mut coeffs = [0i32; 64];
        for i in 0..64 {
            coeffs[i] = spatial[i].round() as i32;
        }
        // Reconstruct the reference pixels the decoder should produce.
        let mut ref_blk = [0f32; 64];
        for i in 0..64 {
            ref_blk[i] = coeffs[i] as f32;
        }
        transform_2d(&mut ref_blk, dct_iii);

        let jpg = arith_seq_gray_8x8(&coeffs);
        let (w, h, dec) = decode_jpeg(&jpg).expect("SOF9 AC block decodes");
        assert_eq!((w, h), (8, 8));
        for r in 0..8 {
            for col in 0..8 {
                let want = (ref_blk[r * 8 + col] + 128.0).round().clamp(0.0, 255.0) as i32;
                let got = dec[(r * 8 + col) * 4] as i32;
                assert!(
                    (got - want).abs() <= 2,
                    "pixel ({r},{col}): want {want}, got {got}"
                );
            }
        }
    }

    /// Assemble a two-scan **SOF10** (progressive arithmetic) grayscale 8×8 JPEG
    /// of a constant block: a DC scan (Ss=0,Se=0) then an AC scan (Ss=1,Se=63),
    /// both Ah=Al=0. Exercises the progressive arithmetic DC-first + AC-first
    /// paths and `Progressive::finish`.
    fn arith_prog_gray_8x8(coeffs: &[i32; 64]) -> Vec<u8> {
        // DC scan entropy.
        let mut dc_enc = MqEncoder::new();
        let mut dc_stats: ArithStats = vec![0u8; 64];
        let mut dc_ctx = 0usize;
        arith_encode_dc(
            &mut dc_enc,
            &mut dc_stats,
            ArithDcCond::default(),
            &mut dc_ctx,
            coeffs[0],
        );
        let dc_entropy = dc_enc.finish();

        // AC scan entropy (fresh statistics — each scan resets them).
        let mut ac_enc = MqEncoder::new();
        let mut ac_stats: ArithStats = vec![0u8; 256];
        let mut sign_bin: ArithStats = vec![0u8; 1];
        arith_encode_ac(
            &mut ac_enc,
            &mut ac_stats,
            &mut sign_bin,
            ArithAcCond::default(),
            coeffs,
            1,
            63,
        );
        let ac_entropy = ac_enc.finish();

        let mut out: Vec<u8> = vec![0xFF, 0xD8]; // SOI
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]); // DQT, all ones
        out.extend_from_slice(&[1u8; 64]);
        // SOF10 — progressive arithmetic, 1 component, 1×1 sampling.
        out.extend_from_slice(&[0xFF, 0xCA, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
        out.extend_from_slice(&[0x01, 0x11, 0x00]);
        // SOS #1 — DC scan: Ss=0 Se=0 Ah=0 Al=0.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&dc_entropy);
        // SOS #2 — AC scan: Ss=1 Se=63 Ah=0 Al=0, AC table id 0.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x01, 0x3F, 0x00]);
        out.extend_from_slice(&ac_entropy);
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        out
    }

    #[test]
    fn decodes_arithmetic_progressive_gray() {
        for &gray in &[160u8, 72] {
            let coeffs = const_block_coeffs(gray);
            let jpg = arith_prog_gray_8x8(&coeffs);
            let (w, h, dec) = decode_jpeg(&jpg).expect("SOF10 progressive decodes");
            assert_eq!((w, h), (8, 8));
            for px in dec.chunks_exact(4) {
                assert!(
                    (px[0] as i32 - gray as i32).abs() <= 3,
                    "gray {gray}: got {}",
                    px[0]
                );
                assert_eq!(px[3], 255);
            }
        }
    }
}
