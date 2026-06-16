//! DES + Triple-DES (EDE3) in CBC mode (FIPS 46-3). Zero dependencies. Needed by
//! legacy PKCS#12 PBES1 (`pbeWithSHAAnd3-KeyTripleDES-CBC`) to decrypt shrouded
//! key bags / encrypted cert bags during user-certificate (P12) import.
//!
//! DES is cryptographically obsolete and used here ONLY to read existing
//! credentials — never to encrypt new data.

// ─── DES permutation / substitution tables (FIPS 46-3) ───────────────────────

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6, 64,
    56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53,
    45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30, 37,
    5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27, 34, 2,
    42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];

const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];

const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19, 13,
    30, 6, 22, 11, 4, 25,
];

const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];

const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1,
        7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14,
        12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8,
        7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12,
        1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13,
        8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14, 2,
        13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5, 15,
        10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8,
        13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Permute `input` (its low `in_bits` bits, DES position 1 = MSB) through
/// `table` (output position → input position, 1-based). Output has `table.len()`
/// bits, MSB first.
fn permute(input: u64, table: &[u8], in_bits: usize) -> u64 {
    let n = table.len();
    let mut out = 0u64;
    for (i, &pos) in table.iter().enumerate() {
        let bit = (input >> (in_bits - pos as usize)) & 1;
        out |= bit << (n - 1 - i);
    }
    out
}

fn be64(bytes: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(a)
}

/// 16 round subkeys (48-bit each) from a 64-bit DES key.
fn key_schedule(key: u64) -> [u64; 16] {
    let permuted = permute(key, &PC1, 64); // 56 bits
    let mut c = (permuted >> 28) & 0x0FFF_FFFF;
    let mut d = permuted & 0x0FFF_FFFF;
    let mut keys = [0u64; 16];
    for (slot, &s) in keys.iter_mut().zip(SHIFTS.iter()) {
        c = ((c << s) | (c >> (28 - s))) & 0x0FFF_FFFF;
        d = ((d << s) | (d >> (28 - s))) & 0x0FFF_FFFF;
        *slot = permute((c << 28) | d, &PC2, 56);
    }
    keys
}

fn feistel(r: u64, k: u64) -> u64 {
    let expanded = permute(r, &E, 32) ^ k; // 48 bits
    let mut out = 0u64; // 32 bits
    for (i, sbox) in SBOX.iter().enumerate() {
        let six = (expanded >> (42 - 6 * i)) & 0x3F;
        let row = ((six >> 5) & 1) << 1 | (six & 1);
        let col = (six >> 1) & 0x0F;
        let val = u64::from(sbox[(row * 16 + col) as usize]);
        out |= val << (28 - 4 * i);
    }
    permute(out, &P, 32)
}

fn des_block(block: u64, keys: &[u64; 16], decrypt: bool) -> u64 {
    let ip = permute(block, &IP, 64);
    let mut l = (ip >> 32) & 0xFFFF_FFFF;
    let mut r = ip & 0xFFFF_FFFF;
    for round in 0..16 {
        let k = if decrypt { keys[15 - round] } else { keys[round] };
        let nl = r;
        r = l ^ feistel(r, k);
        l = nl;
    }
    permute((r << 32) | l, &FP, 64)
}

// ─── Triple-DES (EDE3) CBC ───────────────────────────────────────────────────

struct TripleDes {
    k1: [u64; 16],
    k2: [u64; 16],
    k3: [u64; 16],
}

impl TripleDes {
    /// `key` is 24 bytes (three 8-byte DES keys).
    fn new(key: &[u8]) -> Option<Self> {
        if key.len() != 24 {
            return None;
        }
        Some(Self {
            k1: key_schedule(be64(&key[0..8])),
            k2: key_schedule(be64(&key[8..16])),
            k3: key_schedule(be64(&key[16..24])),
        })
    }

    fn encrypt_block(&self, b: u64) -> u64 {
        des_block(des_block(des_block(b, &self.k1, false), &self.k2, true), &self.k3, false)
    }

    fn decrypt_block(&self, c: u64) -> u64 {
        des_block(des_block(des_block(c, &self.k3, true), &self.k2, false), &self.k1, true)
    }
}

/// 3DES-CBC encrypt. `key` 24 bytes, `iv` 8 bytes, `data` a multiple of 8 bytes.
/// Returns `None` on a bad key/IV/length.
pub fn des3_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let ctx = TripleDes::new(key)?;
    if iv.len() != 8 || !data.len().is_multiple_of(8) {
        return None;
    }
    let mut prev = be64(iv);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(8) {
        let c = ctx.encrypt_block(be64(chunk) ^ prev);
        out.extend_from_slice(&c.to_be_bytes());
        prev = c;
    }
    Some(out)
}

/// 3DES-CBC decrypt. `key` 24 bytes, `iv` 8 bytes, `data` a multiple of 8 bytes.
/// Returns `None` on a bad key/IV/length. PKCS#7 padding is NOT removed here.
pub fn des3_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let ctx = TripleDes::new(key)?;
    if iv.len() != 8 || !data.len().is_multiple_of(8) {
        return None;
    }
    let mut prev = be64(iv);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(8) {
        let c = be64(chunk);
        let p = ctx.decrypt_block(c) ^ prev;
        out.extend_from_slice(&p.to_be_bytes());
        prev = c;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_known_answer() {
        // Classic FIPS test vector.
        let keys = key_schedule(0x1334_5779_9BBC_DFF1);
        let ct = des_block(0x0123_4567_89AB_CDEF, &keys, false);
        assert_eq!(ct, 0x85E8_1354_0F0A_B405);
        // Round-trip.
        assert_eq!(des_block(ct, &keys, true), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn triple_des_with_equal_keys_is_single_des() {
        // EDE with k1==k2==k3 reduces to plain DES — proves the EDE wiring.
        let k = [0x13u8, 0x34, 0x57, 0x79, 0x9B, 0xBC, 0xDF, 0xF1];
        let key24: Vec<u8> = k.iter().chain(&k).chain(&k).copied().collect();
        let ctx = TripleDes::new(&key24).unwrap();
        let single = key_schedule(be64(&k));
        assert_eq!(
            ctx.encrypt_block(0x0123_4567_89AB_CDEF),
            des_block(0x0123_4567_89AB_CDEF, &single, false)
        );
    }

    #[test]
    fn cbc_round_trip() {
        let key: Vec<u8> = (0u8..24).collect();
        let iv = [0xA5u8; 8];
        let data: Vec<u8> = (0u8..32).collect();
        let ct = des3_cbc_encrypt(&key, &iv, &data).unwrap();
        let pt = des3_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(pt, data);
        assert_ne!(ct, data);
    }

    #[test]
    fn rejects_bad_lengths() {
        assert!(des3_cbc_decrypt(&[0; 16], &[0; 8], &[0; 8]).is_none()); // key not 24
        assert!(des3_cbc_decrypt(&[0; 24], &[0; 7], &[0; 8]).is_none()); // iv not 8
        assert!(des3_cbc_decrypt(&[0; 24], &[0; 8], &[0; 7]).is_none()); // data not /8
    }
}
