//! JPEG encoder (baseline) + decoder (baseline **and progressive**) — pure std,
//! zero dependency.
//!
//! The **encoder** emits full-resolution 4:4:4 baseline JPEGs (no chroma
//! subsampling) using the ISO/IEC 10918-1 Annex K example quantization and
//! Huffman tables — enough to re-encode rendered previews/thumbnails. The
//! **decoder** handles both baseline (SOF0) and progressive (SOF2) streams,
//! including successive-approximation refinement, EOB runs, chroma subsampling
//! (nearest-neighbour upsample), and restart markers — the native replacement
//! for a third-party image library's JPEG path. Arithmetic-coded JPEGs
//! (SOF9/10/11) are unsupported and decode to `None` (the caller skips the
//! image rather than blanking the page). Orthonormal float DCT-II / DCT-III
//! (forward/inverse are an exact pair).

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
            // Pad the final partial byte with 1-bits (JPEG convention).
            self.put(0x7F, (8 - self.nbits) as u8);
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
                (&mut cbb, &cq, &mut dc_cb, &tables.dc_chroma, &tables.ac_chroma),
                (&mut crb, &cq, &mut dc_cr, &tables.dc_chroma, &tables.ac_chroma),
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
    out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, hb[2], hb[3], wb[2], wb[3], 0x03]);
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

    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        pos += 2;
        match marker {
            0xD8 | 0xD9 => continue,
            0xC0 | 0xC2 => {
                // SOF0 (baseline) or SOF2 (progressive) — identical layout.
                let l = be16(data, pos) as usize;
                height = be16(data, pos + 3) as u32;
                width = be16(data, pos + 5) as u32;
                let n = data[pos + 7] as usize;
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
                if marker == 0xC2 {
                    // A malformed progressive header aborts the decode.
                    progressive = Some(Progressive::new(width, height, &comps)?);
                }
                pos += l;
            }
            0xC1 | 0xC3 | 0xC9 | 0xCA | 0xCB => return None, // extended/lossless/arithmetic
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
                return decode_scan(data, pos, width, height, &mut comps, &quant, &dc_tabs, &ac_tabs);
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
    let hmax = comps.iter().map(|c| c.h).max()?.max(1) as usize;
    let vmax = comps.iter().map(|c| c.v).max()?.max(1) as usize;
    let mcu_w = 8 * hmax;
    let mcu_h = 8 * vmax;
    let mcus_x = (width as usize).div_ceil(mcu_w);
    let mcus_y = (height as usize).div_ceil(mcu_h);

    // Per-component full-resolution plane (already upsampled to width×height).
    let mut planes: Vec<Vec<f32>> = comps
        .iter()
        .map(|_| vec![0f32; width as usize * height as usize])
        .collect();

    let mut br = BitReader::new(&data[start..]);
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            for (ci, c) in comps.iter_mut().enumerate() {
                for by in 0..c.v as usize {
                    for bx in 0..c.h as usize {
                        let mut block = [0f32; 64];
                        decode_block(&mut br, c, quant, dc_tabs, ac_tabs, &mut block)?;
                        // Place this 8×8 block into the component plane, scaling
                        // its sample footprint to the luma grid (nearest-neighbour
                        // chroma upsample).
                        let sx = hmax / c.h as usize;
                        let sy = vmax / c.v as usize;
                        let ox = (mx * c.h as usize + bx) * 8;
                        let oy = (my * c.v as usize + by) * 8;
                        for r in 0..8 {
                            for col in 0..8 {
                                let val = block[r * 8 + col] + 128.0;
                                for dy in 0..sy {
                                    for dx in 0..sx {
                                        let px = (ox + col) * sx + dx;
                                        let py = (oy + r) * sy + dy;
                                        if px < width as usize && py < height as usize {
                                            planes[ci][py * width as usize + px] = val;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // YCbCr (or grayscale) → RGBA.
    let n = width as usize * height as usize;
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let (r, g, b) = if comps.len() >= 3 {
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
    Some((width, height, out))
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
    fn finish(&self, comps: &[Component], quant: &[[u16; 64]; 4]) -> (u32, u32, Vec<u8>) {
        let (w, h) = (self.width, self.height);
        let mut planes: Vec<Vec<f32>> = self.comps.iter().map(|_| vec![0f32; w * h]).collect();
        for (ci, pc) in self.comps.iter().enumerate() {
            let q = &quant[pc.quant.min(3)];
            let sx = self.hmax / pc.h;
            let sy = self.vmax / pc.v;
            for byk in 0..pc.by {
                for bxk in 0..pc.bx {
                    let blk = &pc.coeffs[byk * pc.bx + bxk];
                    // Dequantize + de-zigzag into natural order.
                    let mut nat = [0f32; 64];
                    for k in 0..64 {
                        nat[ZIGZAG[k]] = blk[k] as f32 * q[ZIGZAG[k]] as f32;
                    }
                    transform_2d(&mut nat, dct_iii);
                    // Place the 8×8 block, scaling each sample to the luma grid.
                    let ox = bxk * 8;
                    let oy = byk * 8;
                    for r in 0..8 {
                        for c in 0..8 {
                            let val = nat[r * 8 + c] + 128.0;
                            for dy in 0..sy {
                                for dx in 0..sx {
                                    let px = (ox + c) * sx + dx;
                                    let py = (oy + r) * sy + dy;
                                    if px < w && py < h {
                                        planes[ci][py * w + px] = val;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let n = w * h;
        let mut out = vec![0u8; n * 4];
        let three = comps.len() >= 3;
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
        (w as u32, h as u32, out)
    }
}

fn be16(d: &[u8], o: usize) -> u16 {
    ((*d.get(o).unwrap_or(&0) as u16) << 8) | *d.get(o + 1).unwrap_or(&0) as u16
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
    fn arithmetic_coded_jpeg_is_rejected_gracefully() {
        // An arithmetic-coded SOF (0xC9) must return None (not panic), so the
        // caller skips the single image rather than blanking the page.
        let mut data: Vec<u8> = vec![0xFF, 0xD8];
        data.extend_from_slice(&[0xFF, 0xC9, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
        data.extend_from_slice(&[0x01, 0x11, 0x00]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        assert!(decode_jpeg(&data).is_none());
    }
}

