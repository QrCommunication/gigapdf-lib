//! Password-based key-derivation functions for native PKCS#12 import:
//!
//! - [`pbkdf2_hmac_sha1`] / [`pbkdf2_hmac_sha256`] — PBKDF2 (RFC 8018 §5.2),
//!   used by modern PBES2-encrypted bags.
//! - [`pkcs12_kdf_sha1`] / [`pkcs12_kdf_sha256`] — the PKCS#12 KDF
//!   (RFC 7292 Appendix B.2), used by legacy PBES1 bags and the integrity MAC.
//! - [`bmp_string`] — password → BMPString (UTF-16BE + trailing 0x0000), the
//!   form the PKCS#12 KDF consumes.
//!
//! Zero dependencies. PBKDF2 is checked against RFC 6070 / RFC 7914 vectors; the
//! PKCS#12 KDF is exercised end-to-end by the OpenSSL P12 round-trip test in the
//! parser (the strongest conformity check — KDF ∘ cipher ∘ ASN.1 together).

use super::hmac::{hmac_sha1, hmac_sha256};
use super::sha1::sha1;
use super::sha256::sha256;

// ─── PBKDF2 (RFC 8018 §5.2) ──────────────────────────────────────────────────

fn pbkdf2(
    prf: impl Fn(&[u8], &[u8]) -> Vec<u8>,
    h_len: usize,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    dk_len: usize,
) -> Vec<u8> {
    let mut dk = Vec::with_capacity(dk_len);
    let blocks = dk_len.div_ceil(h_len) as u32;
    for i in 1..=blocks {
        let mut salt_i = salt.to_vec();
        salt_i.extend_from_slice(&i.to_be_bytes());
        let mut u = prf(password, &salt_i);
        let mut t = u.clone();
        for _ in 1..iterations {
            u = prf(password, &u);
            for (tb, ub) in t.iter_mut().zip(u.iter()) {
                *tb ^= ub;
            }
        }
        dk.extend_from_slice(&t);
    }
    dk.truncate(dk_len);
    dk
}

/// PBKDF2 with HMAC-SHA-1 as the PRF.
pub fn pbkdf2_hmac_sha1(password: &[u8], salt: &[u8], iterations: u32, dk_len: usize) -> Vec<u8> {
    pbkdf2(
        |p, m| hmac_sha1(p, m).to_vec(),
        20,
        password,
        salt,
        iterations,
        dk_len,
    )
}

/// PBKDF2 with HMAC-SHA-256 as the PRF.
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, dk_len: usize) -> Vec<u8> {
    pbkdf2(
        |p, m| hmac_sha256(p, m).to_vec(),
        32,
        password,
        salt,
        iterations,
        dk_len,
    )
}

// ─── PKCS#12 KDF (RFC 7292 Appendix B.2) ─────────────────────────────────────

/// Repeat `data` byte-wise to fill `ceil(len/block)*block` bytes (empty → empty).
fn expand(data: &[u8], block: usize) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let total = data.len().div_ceil(block) * block;
    (0..total).map(|i| data[i % data.len()]).collect()
}

/// `chunk = (chunk + b + 1) mod 2^(8·len)`, big-endian, `chunk.len() == b.len()`.
fn add_one_plus(chunk: &mut [u8], b: &[u8]) {
    let mut carry: u16 = 1;
    for i in (0..chunk.len()).rev() {
        let sum = u16::from(chunk[i]) + u16::from(b[i]) + carry;
        chunk[i] = (sum & 0xff) as u8;
        carry = sum >> 8;
    }
}

fn pkcs12_derive(
    hash: impl Fn(&[u8]) -> Vec<u8>,
    v: usize,
    id: u8,
    pass_bmp: &[u8],
    salt: &[u8],
    iterations: u32,
    n: usize,
) -> Vec<u8> {
    let d = vec![id; v];
    let mut i_block = expand(salt, v);
    i_block.extend_from_slice(&expand(pass_bmp, v));

    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut a = {
            let mut input = d.clone();
            input.extend_from_slice(&i_block);
            hash(&input)
        };
        for _ in 1..iterations.max(1) {
            a = hash(&a);
        }
        out.extend_from_slice(&a);
        if out.len() >= n {
            break;
        }
        // B = A expanded to v bytes; I_j = (I_j + B + 1) for each v-byte block.
        let b = expand(&a, v);
        for chunk in i_block.chunks_mut(v) {
            add_one_plus(chunk, &b);
        }
    }
    out.truncate(n);
    out
}

/// PKCS#12 KDF using SHA-1 (block size 64). `id`: 1 = key, 2 = IV, 3 = MAC.
pub fn pkcs12_kdf_sha1(id: u8, pass_bmp: &[u8], salt: &[u8], iterations: u32, n: usize) -> Vec<u8> {
    pkcs12_derive(|d| sha1(d).to_vec(), 64, id, pass_bmp, salt, iterations, n)
}

/// PKCS#12 KDF using SHA-256 (block size 64). `id`: 1 = key, 2 = IV, 3 = MAC.
pub fn pkcs12_kdf_sha256(
    id: u8,
    pass_bmp: &[u8],
    salt: &[u8],
    iterations: u32,
    n: usize,
) -> Vec<u8> {
    pkcs12_derive(
        |d| sha256(d).to_vec(),
        64,
        id,
        pass_bmp,
        salt,
        iterations,
        n,
    )
}

/// Encode `password` as a PKCS#12 BMPString: UTF-16BE code units followed by a
/// `0x0000` terminator (empty password → `[0x00, 0x00]`).
pub fn bmp_string(password: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(password.len() * 2 + 2);
    for unit in password.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out.extend_from_slice(&[0, 0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc6070_pbkdf2_sha1() {
        assert_eq!(
            hex(&pbkdf2_hmac_sha1(b"password", b"salt", 1, 20)),
            "0c60c80f961f0e71f3a9b524af6012062fe037a6"
        );
        assert_eq!(
            hex(&pbkdf2_hmac_sha1(b"password", b"salt", 2, 20)),
            "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957"
        );
        assert_eq!(
            hex(&pbkdf2_hmac_sha1(b"password", b"salt", 4096, 20)),
            "4b007901b765489abead49d926f721d065a429c1"
        );
    }

    #[test]
    fn pbkdf2_sha256_vectors() {
        assert_eq!(
            hex(&pbkdf2_hmac_sha256(b"password", b"salt", 1, 32)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2_hmac_sha256(b"password", b"salt", 2, 32)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn bmp_string_encoding() {
        assert_eq!(hex(&bmp_string("")), "0000");
        assert_eq!(hex(&bmp_string("ab")), "00610062".to_owned() + "0000");
    }

    #[test]
    fn pkcs12_kdf_is_deterministic_and_purpose_separated() {
        let pass = bmp_string("password");
        let salt = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let key = pkcs12_kdf_sha256(1, &pass, &salt, 2048, 32);
        let iv = pkcs12_kdf_sha256(2, &pass, &salt, 2048, 16);
        assert_eq!(key.len(), 32);
        assert_eq!(iv.len(), 16);
        // Deterministic.
        assert_eq!(key, pkcs12_kdf_sha256(1, &pass, &salt, 2048, 32));
        // Different purpose byte ⇒ different material (no accidental key/IV reuse).
        assert_ne!(&key[..16], &iv[..]);
        // Spanning more than one hash block produces > u bytes correctly.
        assert_eq!(pkcs12_kdf_sha1(1, &pass, &salt, 1, 40).len(), 40);
    }

    #[test]
    fn expand_pads_and_handles_empty() {
        // Empty input → empty output (the early-return branch).
        assert!(expand(&[], 8).is_empty());
        // Non-empty input is repeated byte-wise to fill ceil(len/block)*block.
        assert_eq!(expand(&[1, 2, 3], 4), vec![1, 2, 3, 1]);
        assert_eq!(expand(&[9], 3), vec![9, 9, 9]);
        // Exact multiple stays the same length.
        assert_eq!(expand(&[1, 2, 3, 4], 4), vec![1, 2, 3, 4]);
    }
}
