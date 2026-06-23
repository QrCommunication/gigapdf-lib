//! VP8 (lossy WebP) **keyframe** decoder — pure std, zero dependency.
//!
//! Implements the intra-only keyframe path of VP8 (RFC 6386): the boolean
//! entropy decoder, the frame + macroblock headers, DCT-coefficient token
//! decode, dequantization, the inverse WHT/DCT, intra prediction (16×16, 4×4
//! luma and 8×8 chroma), reconstruction, the normal/simple loop filter, and
//! YUV→RGB. WebP still images are a single keyframe, so this covers lossy WebP
//! decoding without a third-party codec. Inter frames are out of scope.

// ─────────────────────────── boolean entropy decoder ───────────────────────────

/// The arithmetic ("boolean") decoder of RFC 6386 §7. `value` holds ≥8 live bits.
struct Bool<'a> {
    d: &'a [u8],
    pos: usize,
    range: u32,
    value: u32,
    bit_count: i32,
}

impl<'a> Bool<'a> {
    fn new(d: &'a [u8]) -> Bool<'a> {
        let b0 = d.first().copied().unwrap_or(0) as u32;
        let b1 = d.get(1).copied().unwrap_or(0) as u32;
        Bool {
            d,
            pos: 2,
            range: 255,
            value: (b0 << 8) | b1,
            bit_count: 0,
        }
    }

    /// Decode one bit with the given probability (`0..=255`, P(0)).
    fn get(&mut self, prob: u8) -> u32 {
        let split = 1 + (((self.range - 1) * prob as u32) >> 8);
        let big = split << 8;
        let bit;
        if self.value >= big {
            bit = 1;
            self.range -= split;
            self.value -= big;
        } else {
            bit = 0;
            self.range = split;
        }
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;
            self.bit_count += 1;
            if self.bit_count == 8 {
                self.bit_count = 0;
                let next = self.d.get(self.pos).copied().unwrap_or(0) as u32;
                self.pos += 1;
                self.value |= next;
            }
        }
        bit
    }

    /// A bit with even (1/2) probability.
    fn flag(&mut self) -> bool {
        self.get(128) != 0
    }

    /// `n`-bit unsigned literal, MSB first, each bit equiprobable.
    fn literal(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.get(128);
        }
        v
    }

    /// `n`-bit magnitude followed by a sign flag (sign-magnitude), RFC 6386.
    fn signed(&mut self, n: u32) -> i32 {
        let v = self.literal(n) as i32;
        if self.flag() {
            -v
        } else {
            v
        }
    }

    /// Walk a token tree (`tree` is the flattened `[left,right]` pairs as in the
    /// RFC) guided by per-node probabilities, returning the leaf value.
    fn tree(&mut self, tree: &[i8], probs: &[u8]) -> i32 {
        self.tree_from(tree, probs, 0)
    }

    fn tree_from(&mut self, tree: &[i8], probs: &[u8], start: usize) -> i32 {
        let mut i = start;
        loop {
            let b = self.get(probs[i >> 1]) as usize;
            let next = tree[i + b];
            if next <= 0 {
                return (-next) as i32;
            }
            i = next as usize;
        }
    }
}

// ─────────────────────────── constant tables (RFC 6386) ─────────────────────────

/// Coefficient scan order (raster index for each in-zigzag position).
const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Maps a coefficient position (0..16) to its probability band.
const COEFF_BANDS: [usize; 16] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7];

/// DC dequant factors indexed by the (clamped) quant index 0..=127.
const DC_QUANT: [i32; 128] = [
    4, 5, 6, 7, 8, 9, 10, 10, 11, 12, 13, 14, 15, 16, 17, 17, 18, 19, 20, 20, 21, 21, 22, 22, 23,
    23, 24, 25, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 37, 38, 39, 40, 41, 42, 43, 44,
    45, 46, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67,
    68, 69, 70, 71, 72, 73, 74, 75, 76, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 91,
    93, 95, 96, 98, 100, 101, 102, 104, 106, 108, 110, 112, 114, 116, 118, 122, 124, 126, 128, 130,
    132, 134, 136, 138, 140, 143, 145, 148, 151, 154, 157,
];

/// AC dequant factors indexed by the (clamped) quant index 0..=127.
const AC_QUANT: [i32; 128] = [
    4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
    29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52,
    53, 54, 55, 56, 57, 58, 60, 62, 64, 66, 68, 70, 72, 74, 76, 78, 80, 82, 84, 86, 88, 90, 92, 94,
    96, 98, 100, 102, 104, 106, 108, 110, 112, 114, 116, 119, 122, 125, 128, 131, 134, 137, 140,
    143, 146, 149, 152, 155, 158, 161, 164, 167, 170, 173, 177, 181, 185, 189, 193, 197, 201, 205,
    209, 213, 217, 221, 225, 229, 234, 239, 245, 249, 254, 259, 264, 269, 274, 279, 284,
];

// The DCT-token tree and category extra-bit probabilities are decoded inline in
// `vp8/decoder.rs` (its `decode_block` walks the tree and `CAT_PROBS` holds the
// dixie-order category probabilities), so no flattened tree table is needed here.

// ── intra prediction mode trees / probabilities ──

// 16×16 luma modes.
const DC_PRED: i32 = 0;
const V_PRED: i32 = 1;
const H_PRED: i32 = 2;
const TM_PRED: i32 = 3;
const B_PRED: i32 = 4;

const KF_YMODE_TREE: [i8; 8] = [
    -(B_PRED as i8),
    2,
    4,
    6,
    -(DC_PRED as i8),
    -(V_PRED as i8),
    -(H_PRED as i8),
    -(TM_PRED as i8),
];
const KF_YMODE_PROB: [u8; 4] = [145, 156, 163, 128];

const KF_UV_MODE_TREE: [i8; 6] = [
    -(DC_PRED as i8),
    2,
    -(V_PRED as i8),
    4,
    -(H_PRED as i8),
    -(TM_PRED as i8),
];
const KF_UV_MODE_PROB: [u8; 3] = [142, 114, 183];

// 4×4 luma sub-block modes (B_PRED).
const B_DC_PRED: i32 = 0;
const B_TM_PRED: i32 = 1;
const B_VE_PRED: i32 = 2;
const B_HE_PRED: i32 = 3;
const B_LD_PRED: i32 = 4;
const B_RD_PRED: i32 = 5;
const B_VR_PRED: i32 = 6;
const B_VL_PRED: i32 = 7;
const B_HD_PRED: i32 = 8;
const B_HU_PRED: i32 = 9;

const BMODE_TREE: [i8; 18] = [
    -(B_DC_PRED as i8),
    2,
    -(B_TM_PRED as i8),
    4,
    -(B_VE_PRED as i8),
    6,
    8,
    12,
    -(B_HE_PRED as i8),
    10,
    -(B_RD_PRED as i8),
    -(B_VR_PRED as i8),
    -(B_LD_PRED as i8),
    14,
    -(B_VL_PRED as i8),
    16,
    -(B_HD_PRED as i8),
    -(B_HU_PRED as i8),
];

include!("vp8_tables.rs");

// ─────────────────────────── decoder state ─────────────────────────────────────

/// Decode a VP8 keyframe chunk body (`VP8 ` payload) to `(width, height, rgba)`.
pub fn decode(body: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    decoder::decode_keyframe(body)
}

mod decoder;
