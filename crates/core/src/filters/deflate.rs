//! A DEFLATE / zlib **encoder** (RFC 1951/1950) — zero dependencies.
//!
//! The decoder lives in [`super::inflate`]; this is the missing other half,
//! needed to compress PDF streams and to build Office (OOXML/ODF) ZIP archives.
//! It emits a single fixed-Huffman block with LZ77 back-references found via a
//! hash chain — simple, correct, and a solid size win on text. Correctness is
//! pinned by round-tripping through our own inflate.

// Length codes 257..=285: (base length, extra bits).
const LENGTH_TABLE: [(u16, u8); 29] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 1),
    (13, 1),
    (15, 1),
    (17, 1),
    (19, 2),
    (23, 2),
    (27, 2),
    (31, 2),
    (35, 3),
    (43, 3),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 4),
    (115, 4),
    (131, 5),
    (163, 5),
    (195, 5),
    (227, 5),
    (258, 0),
];

// Distance codes 0..=29: (base distance, extra bits).
const DIST_TABLE: [(u16, u8); 30] = [
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 1),
    (7, 1),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 4),
    (49, 4),
    (65, 5),
    (97, 5),
    (129, 6),
    (193, 6),
    (257, 7),
    (385, 7),
    (513, 8),
    (769, 8),
    (1025, 9),
    (1537, 9),
    (2049, 10),
    (3073, 10),
    (4097, 11),
    (6145, 11),
    (8193, 12),
    (12289, 12),
    (16385, 13),
    (24577, 13),
];

struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    bits: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            buf: 0,
            bits: 0,
        }
    }

    /// Write the low `count` bits of `value`, least-significant bit first
    /// (DEFLATE's packing order for data elements and extra bits).
    fn write_bits(&mut self, value: u32, count: u32) {
        let mask = if count >= 32 {
            u32::MAX
        } else {
            (1u32 << count) - 1
        };
        self.buf |= ((value & mask) as u64) << self.bits;
        self.bits += count;
        while self.bits >= 8 {
            self.out.push((self.buf & 0xFF) as u8);
            self.buf >>= 8;
            self.bits -= 8;
        }
    }

    /// Write a Huffman `code` of `len` bits, most-significant bit first (the
    /// order DEFLATE packs Huffman codes in).
    fn write_code(&mut self, code: u32, len: u32) {
        let mut reversed = 0u32;
        for i in 0..len {
            reversed |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.write_bits(reversed, len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push((self.buf & 0xFF) as u8);
        }
        self.out
    }
}

/// Fixed-Huffman literal/length code for `sym` (RFC 1951 §3.2.6): `(code, len)`.
fn litlen_code(sym: u16) -> (u32, u32) {
    match sym {
        0..=143 => (0b0011_0000 + sym as u32, 8),
        144..=255 => (0b1_1001_0000 + (sym as u32 - 144), 9),
        256..=279 => (sym as u32 - 256, 7),
        _ => (0b1100_0000 + (sym as u32 - 280), 8),
    }
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn emit_literal(bw: &mut BitWriter, byte: u8) {
    let (code, len) = litlen_code(byte as u16);
    bw.write_code(code, len);
}

fn emit_match(bw: &mut BitWriter, length: usize, distance: usize) {
    // Length: find its symbol + extra bits.
    let li = LENGTH_TABLE
        .iter()
        .rposition(|&(base, _)| base as usize <= length)
        .unwrap_or(0);
    let (lbase, lextra) = LENGTH_TABLE[li];
    let (code, clen) = litlen_code(257 + li as u16);
    bw.write_code(code, clen);
    if lextra > 0 {
        bw.write_bits((length - lbase as usize) as u32, lextra as u32);
    }
    // Distance: 5-bit fixed code + extra bits.
    let di = DIST_TABLE
        .iter()
        .rposition(|&(base, _)| base as usize <= distance)
        .unwrap_or(0);
    let (dbase, dextra) = DIST_TABLE[di];
    bw.write_code(di as u32, 5);
    if dextra > 0 {
        bw.write_bits((distance - dbase as usize) as u32, dextra as u32);
    }
}

const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MAX_PROBES: usize = 128;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const WINDOW: usize = 32768;

fn hash3(data: &[u8], i: usize) -> usize {
    ((data[i] as usize) << 10 ^ (data[i + 1] as usize) << 5 ^ data[i + 2] as usize)
        & (HASH_SIZE - 1)
}

/// Raw DEFLATE stream (single fixed-Huffman block) for `data`.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write_bits(1, 1); // BFINAL = 1
    bw.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)

    let mut head = vec![-1i32; HASH_SIZE];
    let mut prev = vec![-1i32; data.len().max(1)];

    let mut i = 0;
    while i < data.len() {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + MIN_MATCH <= data.len() {
            let h = hash3(data, i);
            let mut j = head[h];
            let mut probes = MAX_PROBES;
            while j >= 0 && probes > 0 {
                let jp = j as usize;
                let dist = i - jp;
                if dist > WINDOW {
                    break;
                }
                let mut len = 0;
                while len < MAX_MATCH && i + len < data.len() && data[jp + len] == data[i + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = dist;
                    if len >= MAX_MATCH {
                        break;
                    }
                }
                j = prev[jp];
                probes -= 1;
            }
        }

        let advance = if best_len >= MIN_MATCH {
            emit_match(&mut bw, best_len, best_dist);
            best_len
        } else {
            emit_literal(&mut bw, data[i]);
            1
        };
        for k in 0..advance {
            let p = i + k;
            if p + MIN_MATCH <= data.len() {
                let h = hash3(data, p);
                prev[p] = head[h];
                head[h] = p as i32;
            }
        }
        i += advance;
    }

    let (code, len) = litlen_code(256); // end of block
    bw.write_code(code, len);
    bw.finish()
}

/// zlib-wrapped (RFC 1950) DEFLATE of `data`: header + deflate + Adler-32.
pub fn flate_encode(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::inflate::{flate_decode, inflate};

    #[test]
    fn deflate_round_trips_through_inflate() {
        let cases: &[&[u8]] = &[
            b"",
            b"a",
            b"hello hello hello hello world",
            b"The quick brown fox jumps over the lazy dog. The quick brown fox.",
        ];
        for &data in cases {
            assert_eq!(
                inflate(&deflate(data)).unwrap(),
                data,
                "raw deflate {data:?}"
            );
            assert_eq!(
                flate_decode(&flate_encode(data)).unwrap(),
                data,
                "zlib {data:?}"
            );
        }
    }

    #[test]
    fn compresses_repetitive_data() {
        let data = vec![b'A'; 4096];
        let encoded = flate_encode(&data);
        assert!(
            encoded.len() < data.len() / 4,
            "highly repetitive data shrinks"
        );
        assert_eq!(flate_decode(&encoded).unwrap(), data);
    }

    #[test]
    fn round_trips_binary() {
        let data: Vec<u8> = (0..2000).map(|i| (i * 31 + 7) as u8).collect();
        assert_eq!(flate_decode(&flate_encode(&data)).unwrap(), data);
    }

    #[test]
    fn write_bits_full_word_mask() {
        // count == 32 takes the `u32::MAX` mask branch (the `count >= 32` guard);
        // writing 32 bits then flushing yields exactly the 4 little-endian bytes.
        let mut bw = BitWriter::new();
        bw.write_bits(0xDEAD_BEEF, 32);
        let out = bw.finish();
        assert_eq!(out, vec![0xEF, 0xBE, 0xAD, 0xDE]);
    }
}
