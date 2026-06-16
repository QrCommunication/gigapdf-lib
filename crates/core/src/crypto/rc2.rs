//! RC2 block cipher (RFC 2268) in CBC mode — zero dependencies.
//!
//! Needed only to read **legacy** PKCS#12 files: OpenSSL < 3 (and the `-legacy`
//! flag) encrypt the certificate bags with `pbeWithSHAAnd40BitRC2-CBC`. RC2-40
//! is a weak, deprecated cipher — it is implemented purely to import such a
//! `.p12`, never to protect anything. Modern files use PBES2/AES.
//!
//! Conformity is pinned by the RFC 2268 §5 known-answer vectors.

/// The RC2 permutation table (PITABLE, RFC 2268 §2).
#[rustfmt::skip]
const PITABLE: [u8; 256] = [
    0xd9, 0x78, 0xf9, 0xc4, 0x19, 0xdd, 0xb5, 0xed, 0x28, 0xe9, 0xfd, 0x79, 0x4a, 0xa0, 0xd8, 0x9d,
    0xc6, 0x7e, 0x37, 0x83, 0x2b, 0x76, 0x53, 0x8e, 0x62, 0x4c, 0x64, 0x88, 0x44, 0x8b, 0xfb, 0xa2,
    0x17, 0x9a, 0x59, 0xf5, 0x87, 0xb3, 0x4f, 0x13, 0x61, 0x45, 0x6d, 0x8d, 0x09, 0x81, 0x7d, 0x32,
    0xbd, 0x8f, 0x40, 0xeb, 0x86, 0xb7, 0x7b, 0x0b, 0xf0, 0x95, 0x21, 0x22, 0x5c, 0x6b, 0x4e, 0x82,
    0x54, 0xd6, 0x65, 0x93, 0xce, 0x60, 0xb2, 0x1c, 0x73, 0x56, 0xc0, 0x14, 0xa7, 0x8c, 0xf1, 0xdc,
    0x12, 0x75, 0xca, 0x1f, 0x3b, 0xbe, 0xe4, 0xd1, 0x42, 0x3d, 0xd4, 0x30, 0xa3, 0x3c, 0xb6, 0x26,
    0x6f, 0xbf, 0x0e, 0xda, 0x46, 0x69, 0x07, 0x57, 0x27, 0xf2, 0x1d, 0x9b, 0xbc, 0x94, 0x43, 0x03,
    0xf8, 0x11, 0xc7, 0xf6, 0x90, 0xef, 0x3e, 0xe7, 0x06, 0xc3, 0xd5, 0x2f, 0xc8, 0x66, 0x1e, 0xd7,
    0x08, 0xe8, 0xea, 0xde, 0x80, 0x52, 0xee, 0xf7, 0x84, 0xaa, 0x72, 0xac, 0x35, 0x4d, 0x6a, 0x2a,
    0x96, 0x1a, 0xd2, 0x71, 0x5a, 0x15, 0x49, 0x74, 0x4b, 0x9f, 0xd0, 0x5e, 0x04, 0x18, 0xa4, 0xec,
    0xc2, 0xe0, 0x41, 0x6e, 0x0f, 0x51, 0xcb, 0xcc, 0x24, 0x91, 0xaf, 0x50, 0xa1, 0xf4, 0x70, 0x39,
    0x99, 0x7c, 0x3a, 0x85, 0x23, 0xb8, 0xb4, 0x7a, 0xfc, 0x02, 0x36, 0x5b, 0x25, 0x55, 0x97, 0x31,
    0x2d, 0x5d, 0xfa, 0x98, 0xe3, 0x8a, 0x92, 0xae, 0x05, 0xdf, 0x29, 0x10, 0x67, 0x6c, 0xba, 0xc9,
    0xd3, 0x00, 0xe6, 0xcf, 0xe1, 0x9e, 0xa8, 0x2c, 0x63, 0x16, 0x01, 0x3f, 0x58, 0xe2, 0x89, 0xa9,
    0x0d, 0x38, 0x34, 0x1b, 0xab, 0x33, 0xff, 0xb0, 0xbb, 0x48, 0x0c, 0x5f, 0xb9, 0xb1, 0xcd, 0x2e,
    0xc5, 0xf3, 0xdb, 0x47, 0xe5, 0xa5, 0x9c, 0x77, 0x0a, 0xa6, 0x20, 0x68, 0xfe, 0x7f, 0xc1, 0xad,
];

/// Per-word left-rotation amounts for the mixing rounds (RFC 2268 §3).
const S: [u32; 4] = [1, 2, 3, 5];

/// Expand a variable-length `key` (with `effective_bits` of effective strength)
/// into the 64-word RC2 key schedule (RFC 2268 §2).
fn expand_key(key: &[u8], effective_bits: usize) -> [u16; 64] {
    let mut l = [0u8; 128];
    let t = key.len().min(128);
    l[..t].copy_from_slice(&key[..t]);
    for i in t..128 {
        l[i] = PITABLE[(l[i - 1] as usize + l[i - t] as usize) & 0xff];
    }
    let t8 = effective_bits.div_ceil(8);
    let mask_bits = 8 + effective_bits - 8 * t8; // in 1..=8
    let tm = (255u16 % (1u16 << mask_bits)) as u8;
    l[128 - t8] = PITABLE[(l[128 - t8] & tm) as usize];
    for i in (0..128 - t8).rev() {
        l[i] = PITABLE[(l[i + 1] ^ l[i + t8]) as usize];
    }
    let mut k = [0u16; 64];
    for (i, word) in k.iter_mut().enumerate() {
        *word = l[2 * i] as u16 | ((l[2 * i + 1] as u16) << 8);
    }
    k
}

// Encryption (`mix`/`mash`/`encrypt_block`) exists only to drive the RFC 2268
// known-answer tests — GigaPDF never RC2-*encrypts*, it only decrypts a legacy
// `.p12`. Gated to the test build so it isn't dead code in production.
#[cfg(test)]
fn mix(r: &mut [u16; 4], k: &[u16; 64], j: &mut usize) {
    for x in 0..4 {
        let a = r[(x + 3) & 3]; // R[x-1]
        let b = r[(x + 2) & 3]; // R[x-2]
        let c = r[(x + 1) & 3]; // R[x-3]
        r[x] = r[x]
            .wrapping_add(k[*j])
            .wrapping_add(a & b)
            .wrapping_add(!a & c);
        *j += 1;
        r[x] = r[x].rotate_left(S[x]);
    }
}

#[cfg(test)]
fn mash(r: &mut [u16; 4], k: &[u16; 64]) {
    for x in 0..4 {
        let idx = (r[(x + 3) & 3] & 63) as usize;
        r[x] = r[x].wrapping_add(k[idx]);
    }
}

fn r_mix(r: &mut [u16; 4], k: &[u16; 64], j: &mut usize) {
    for x in (0..4).rev() {
        r[x] = r[x].rotate_right(S[x]);
        let a = r[(x + 3) & 3];
        let b = r[(x + 2) & 3];
        let c = r[(x + 1) & 3];
        r[x] = r[x]
            .wrapping_sub(k[*j])
            .wrapping_sub(a & b)
            .wrapping_sub(!a & c);
        *j = j.wrapping_sub(1);
    }
}

fn r_mash(r: &mut [u16; 4], k: &[u16; 64]) {
    for x in (0..4).rev() {
        let idx = (r[(x + 3) & 3] & 63) as usize;
        r[x] = r[x].wrapping_sub(k[idx]);
    }
}

#[cfg(test)]
fn encrypt_block(r: &mut [u16; 4], k: &[u16; 64]) {
    let mut j = 0;
    for _ in 0..5 {
        mix(r, k, &mut j);
    }
    mash(r, k);
    for _ in 0..6 {
        mix(r, k, &mut j);
    }
    mash(r, k);
    for _ in 0..5 {
        mix(r, k, &mut j);
    }
}

fn decrypt_block(r: &mut [u16; 4], k: &[u16; 64]) {
    let mut j = 63;
    for _ in 0..5 {
        r_mix(r, k, &mut j);
    }
    r_mash(r, k);
    for _ in 0..6 {
        r_mix(r, k, &mut j);
    }
    r_mash(r, k);
    for _ in 0..5 {
        r_mix(r, k, &mut j);
    }
}

fn block_to_words(b: &[u8]) -> [u16; 4] {
    [
        b[0] as u16 | ((b[1] as u16) << 8),
        b[2] as u16 | ((b[3] as u16) << 8),
        b[4] as u16 | ((b[5] as u16) << 8),
        b[6] as u16 | ((b[7] as u16) << 8),
    ]
}

fn words_to_block(r: &[u16; 4]) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (i, &w) in r.iter().enumerate() {
        out[2 * i] = (w & 0xff) as u8;
        out[2 * i + 1] = (w >> 8) as u8;
    }
    out
}

/// Decrypt `data` (RC2-CBC) with `key`/`effective_bits` and an 8-byte `iv`.
/// `None` if the IV or data length is invalid.
pub fn rc2_cbc_decrypt(
    key: &[u8],
    effective_bits: usize,
    iv: &[u8],
    data: &[u8],
) -> Option<Vec<u8>> {
    if iv.len() != 8 || data.is_empty() || !data.len().is_multiple_of(8) {
        return None;
    }
    let k = expand_key(key, effective_bits);
    let mut out = Vec::with_capacity(data.len());
    let mut prev = [0u8; 8];
    prev.copy_from_slice(iv);
    for block in data.chunks_exact(8) {
        let mut r = block_to_words(block);
        decrypt_block(&mut r, &k);
        let dec = words_to_block(&r);
        for i in 0..8 {
            out.push(dec[i] ^ prev[i]);
        }
        prev.copy_from_slice(block);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: &[u8], bits: usize, pt: [u8; 8]) -> [u8; 8] {
        let k = expand_key(key, bits);
        let mut r = block_to_words(&pt);
        encrypt_block(&mut r, &k);
        words_to_block(&r)
    }

    #[test]
    fn rfc2268_known_answer_vectors() {
        // (key, T1, plaintext, ciphertext) from RFC 2268 §5.
        assert_eq!(
            enc(&[0u8; 8], 63, [0u8; 8]),
            [0xeb, 0xb7, 0x73, 0xf9, 0x93, 0x27, 0x8e, 0xff]
        );
        assert_eq!(
            enc(&[0xffu8; 8], 64, [0xffu8; 8]),
            [0x27, 0x8b, 0x27, 0xe4, 0x2e, 0x2f, 0x0d, 0x49]
        );
        assert_eq!(
            enc(
                &[0x30, 0, 0, 0, 0, 0, 0, 0],
                64,
                [0x10, 0, 0, 0, 0, 0, 0, 0x01]
            ),
            [0x30, 0x64, 0x9e, 0xdf, 0x9b, 0xe7, 0xd2, 0xc2]
        );
        assert_eq!(
            enc(&[0x88], 64, [0u8; 8]),
            [0x61, 0xa8, 0xa2, 0x44, 0xad, 0xac, 0xcc, 0xf0]
        );
        assert_eq!(
            enc(&[0x88, 0xbc, 0xa9, 0x0e, 0x90, 0x87, 0x5a], 64, [0u8; 8]),
            [0x6c, 0xcf, 0x43, 0x08, 0x97, 0x4c, 0x26, 0x7f]
        );
        let key16 = [
            0x88, 0xbc, 0xa9, 0x0e, 0x90, 0x87, 0x5a, 0x7f, 0x0f, 0x79, 0xc3, 0x84, 0x62, 0x7b,
            0xaf, 0xb2,
        ];
        assert_eq!(
            enc(&key16, 128, [0u8; 8]),
            [0x22, 0x69, 0x55, 0x2a, 0xb0, 0xf8, 0x5c, 0xa6]
        );
    }

    #[test]
    fn cbc_round_trips_at_40_bits() {
        // The RC2-40 case PKCS#12 legacy cert bags use.
        let key = [0x12u8, 0x34, 0x56, 0x78, 0x9a]; // 40-bit
        let iv = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let plain = b"GigaPDF legacy RC2 round-trip!!\x00"; // 32 bytes (multiple of 8)

        // Encrypt with CBC manually, then decrypt and compare.
        let k = expand_key(&key, 40);
        let mut prev = iv;
        let mut ct = Vec::new();
        for block in plain.chunks_exact(8) {
            let mut xored = [0u8; 8];
            for i in 0..8 {
                xored[i] = block[i] ^ prev[i];
            }
            let mut r = block_to_words(&xored);
            encrypt_block(&mut r, &k);
            let enc = words_to_block(&r);
            ct.extend_from_slice(&enc);
            prev = enc;
        }
        let recovered = rc2_cbc_decrypt(&key, 40, &iv, &ct).unwrap();
        assert_eq!(&recovered, plain);
    }

    #[test]
    fn rejects_bad_lengths() {
        assert!(rc2_cbc_decrypt(&[0u8; 5], 40, &[0u8; 7], &[0u8; 8]).is_none()); // IV ≠ 8
        assert!(rc2_cbc_decrypt(&[0u8; 5], 40, &[0u8; 8], &[0u8; 7]).is_none()); // not /8
    }
}
